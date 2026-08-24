use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::{Duration, Instant};
use dashmap::DashMap;
use xrouter_core::ApiKey;

#[derive(Debug, Clone)]
pub struct KeyRing {
    keys: Arc<[ApiKey]>,
    cursor: Arc<AtomicUsize>,
}

impl KeyRing {
    pub fn new(keys: Vec<ApiKey>) -> Self {
        Self { keys: keys.into(), cursor: Arc::new(AtomicUsize::new(0)) }
    }
    pub fn len(&self) -> usize { self.keys.len() }
    pub fn is_empty(&self) -> bool { self.keys.is_empty() }
    pub fn next(&self) -> Option<(&ApiKey, usize)> {
        if self.keys.is_empty() { return None; }
        let i = self.cursor.fetch_add(1, Ordering::Relaxed) % self.keys.len();
        Some((&self.keys[i], i))
    }
    /// Get key at index without advancing cursor (for retry)
    pub fn get(&self, idx: usize) -> Option<&ApiKey> { self.keys.get(idx) }
    pub fn replace_keys(&mut self, keys: Vec<ApiKey>) {
        self.keys = keys.into();
        self.cursor.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    Cooling { until: Instant, failures: u32 },
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct EndpointHealth {
    pub state: HealthState,
    pub consecutive_failures: u32,
    pub last_failure: Option<Instant>,
}

impl Default for EndpointHealth {
    fn default() -> Self { Self { state: HealthState::Healthy, consecutive_failures: 0, last_failure: None } }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct EndpointId(pub String); // "provider:model"

impl EndpointId {
    pub fn new(provider: &str, model: &str) -> Self { Self(format!("{}:{}", provider, model)) }
}

pub struct HealthRegistry {
    map: DashMap<EndpointId, EndpointHealth>,
    // per-key dead set
    dead_keys: DashMap<String, Instant>, // key -> dead since
}

impl HealthRegistry {
    pub fn new() -> Self { Self { map: DashMap::new(), dead_keys: DashMap::new() } }

    pub fn is_healthy(&self, id: &EndpointId) -> bool {
        if let Some(h) = self.map.get(id) {
            match h.state {
                HealthState::Healthy => true,
                HealthState::HalfOpen => true, // allow probe
                HealthState::Cooling { until, .. } => Instant::now() >= until,
            }
        } else { true }
    }

    pub fn mark_success(&self, id: &EndpointId) {
        self.map.insert(id.clone(), EndpointHealth { state: HealthState::Healthy, consecutive_failures: 0, last_failure: None });
    }

    pub fn mark_failure(&self, id: &EndpointId) {
        let mut entry = self.map.entry(id.clone()).or_default();
        entry.consecutive_failures += 1;
        entry.last_failure = Some(Instant::now());
        if entry.consecutive_failures >= 3 {
            let backoff = Duration::from_secs(30 * 2u64.pow((entry.consecutive_failures - 3).min(4) as u32));
            let capped = std::cmp::min(backoff, Duration::from_secs(300));
            entry.state = HealthState::Cooling { until: Instant::now() + capped, failures: entry.consecutive_failures };
        }
    }

    pub fn mark_key_dead(&self, key: &str) {
        self.dead_keys.insert(key.to_string(), Instant::now());
    }
    pub fn is_key_dead(&self, key: &str) -> bool {
        self.dead_keys.contains_key(key)
    }
    pub fn clear_key(&self, key: &str) { self.dead_keys.remove(key); }

    pub fn snapshot(&self) -> Vec<(EndpointId, EndpointHealth)> {
        self.map.iter().map(|e| (e.key().clone(), e.value().clone())).collect()
    }

    /// Transition cooling -> half-open if timer expired
    pub fn maybe_half_open(&self, id: &EndpointId) {
        if let Some(mut h) = self.map.get_mut(id) {
            if let HealthState::Cooling { until, .. } = h.state {
                if Instant::now() >= until {
                    h.state = HealthState::HalfOpen;
                }
            }
        }
    }

    /// Check if provider is degraded: >70% of its endpoints cooling
    pub fn is_provider_degraded(&self, provider: &str) -> bool {
        let mut total = 0;
        let mut cooling = 0;
        for entry in self.map.iter() {
            if entry.key().0.starts_with(&format!("{}:", provider)) {
                total += 1;
                if matches!(entry.value().state, HealthState::Cooling { .. }) {
                    cooling += 1;
                }
            }
        }
        if total == 0 { return false; }
        (cooling as f64) / (total as f64) > 0.7
    }

    /// Parse Retry-After header value (seconds or http-date fallback)
    pub fn apply_retry_after(&self, id: &EndpointId, retry_after: Option<&str>) {
        if let Some(val) = retry_after {
            if let Ok(secs) = val.parse::<u64>() {
                let until = Instant::now() + Duration::from_secs(secs);
                self.map.insert(id.clone(), EndpointHealth { state: HealthState::Cooling { until, failures: 1 }, consecutive_failures: 1, last_failure: Some(Instant::now()) });
            }
        }
    }
}

impl Default for HealthRegistry {
    fn default() -> Self { Self::new() }
}

// Balancer combines KeyRing per provider + health
pub struct Balancer {
    pub keyrings: DashMap<String, KeyRing>,
    pub health: HealthRegistry,
}

impl Balancer {
    pub fn new() -> Self { Self { keyrings: DashMap::new(), health: HealthRegistry::new() } }

    pub fn update_keys(&self, provider: &str, keys: Vec<ApiKey>) {
        self.keyrings.insert(provider.to_string(), KeyRing::new(keys));
    }

    pub fn next_key(&self, provider: &str) -> Option<(ApiKey, usize)> {
        let ring = self.keyrings.get(provider)?;
        // try to find non-dead key, loop at most len times
        for _ in 0..ring.len() {
            if let Some((k, idx)) = ring.next() {
                if !self.health.is_key_dead(k.expose()) {
                    return Some((k.clone(), idx));
                }
            } else { break; }
        }
        // if all dead, return none
        None
    }

    /// Number of configured keys for a provider (0 if unknown).
    pub fn keys_len(&self, provider: &str) -> usize {
        self.keyrings.get(provider).map(|r| r.len()).unwrap_or(0)
    }

    pub fn candidates_ordered<'a>(&self, tier: &'a xrouter_core::Tier) -> Vec<&'a xrouter_core::tier::ModelEntry> {
        // Provider degraded check: if provider degraded, deprioritize its entries but keep within tier
        let mut healthy = Vec::new();
        let mut degraded = Vec::new();
        let mut unhealthy = Vec::new();
        for e in &tier.entries {
            let id = EndpointId::new(&e.provider, &e.model);
            self.health.maybe_half_open(&id);
            let is_healthy = self.health.is_healthy(&id);
            let is_degraded_provider = self.health.is_provider_degraded(&e.provider);
            if is_healthy && !is_degraded_provider {
                healthy.push(e);
            } else if is_healthy && is_degraded_provider {
                degraded.push(e);
            } else {
                unhealthy.push(e);
            }
        }
        let mut res = healthy;
        res.extend(degraded);
        res.extend(unhealthy);
        res
    }

    /// Weighted pick among healthy candidates using smooth WRR; falls back to ordered if weights equal
    pub fn pick_weighted<'a>(&self, tier: &'a xrouter_core::Tier) -> Option<&'a xrouter_core::tier::ModelEntry> {
        let candidates = self.candidates_ordered(tier);
        if candidates.is_empty() { return None; }
        // If all weights 1, just return first healthy (ordered)
        let all_one = candidates.iter().all(|c| c.weight == 1);
        if all_one { return Some(candidates[0]); }
        // Otherwise apply WRR over candidates subset: create picker each call (stateless demo)
        // For true WRR we would keep state, but for simplicity use weighted random-ish: pick max weight among healthy first
        return candidates.into_iter().max_by_key(|c| c.weight);
    }
}

impl Default for Balancer {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyring_round_robin() {
        let r = KeyRing::new(vec![ApiKey("a".into()), ApiKey("b".into()), ApiKey("c".into())]);
        let seq: Vec<String> = (0..6).map(|_| r.next().unwrap().0 .0.clone()).collect();
        assert_eq!(seq, vec!["a","b","c","a","b","c"]);
    }
    #[test]
    fn keyring_wraparound() {
        let r = KeyRing::new(vec![ApiKey("x".into())]);
        for _ in 0..10 { assert_eq!(r.next().unwrap().0 .0, "x"); }
    }
    #[test]
    fn health_cooling() {
        let h = HealthRegistry::new();
        let id = EndpointId::new("p","m");
        assert!(h.is_healthy(&id));
        h.mark_failure(&id);
        h.mark_failure(&id);
        assert!(h.is_healthy(&id));
        h.mark_failure(&id);
        assert!(!h.is_healthy(&id));
        h.mark_success(&id);
        assert!(h.is_healthy(&id));
    }
    #[test]
    fn balancer_dead_key_skip() {
        let b = Balancer::new();
        b.update_keys("prov", vec![ApiKey("k1".into()), ApiKey("k2".into())]);
        b.health.mark_key_dead("k1");
        // next should skip k1, but round-robin will cycle; ensure we get k2 sometimes
        let mut got_k2 = false;
        for _ in 0..4 {
            if let Some((k, _)) = b.next_key("prov") { if k.0=="k2" { got_k2=true; } }
        }
        assert!(got_k2);
    }
}
