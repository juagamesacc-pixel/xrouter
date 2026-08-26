use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use dashmap::DashMap;
use rand::RngExt;
use xrouter_core::ApiKey;
use xrouter_core::tier::ModelEntry;

/// `EndpointId` is defined in `xrouter-core` so that `ModelEntry` can own one
/// without a crate dependency cycle. Re-export it here for backwards compat.
pub use xrouter_core::EndpointId;

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

pub struct HealthRegistry {
    map: DashMap<EndpointId, EndpointHealth>,
    // per-key dead set
    dead_keys: DashMap<String, Instant>, // key -> dead since
    // O(1) provider degradation tracking: number of endpoints currently in
    // `Cooling` and the total number of distinct endpoints ever seen, per
    // provider. Updated only on state transitions (no hot-path scan).
    cooling_counts: DashMap<String, AtomicUsize>,
    total_counts: DashMap<String, AtomicUsize>,
}

fn inc_counter(map: &DashMap<String, AtomicUsize>, key: &str) {
    map.entry(key.to_string()).or_insert_with(|| AtomicUsize::new(0)).fetch_add(1, Ordering::Relaxed);
}

fn dec_counter(map: &DashMap<String, AtomicUsize>, key: &str) {
    if let Some(c) = map.get(key) {
        let _ = c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            map: DashMap::new(),
            dead_keys: DashMap::new(),
            cooling_counts: DashMap::new(),
            total_counts: DashMap::new(),
        }
    }

    /// Ensure the provider's total-endpoint counter is registered the first
    /// time we observe an endpoint.
    fn register(&self, id: &EndpointId) {
        if !self.map.contains_key(id) {
            inc_counter(&self.total_counts, id.provider());
        }
    }

    /// Single-pass health read for one endpoint. Also transitions an expired
    /// `Cooling` endpoint into `HalfOpen` (so idle tiers recover without an
    /// external probe) and decrements the provider's cooling counter. This makes
    /// `is_healthy` self-healing: any health read advances the state machine.
    pub fn health_state(&self, id: &EndpointId) -> HealthState {
        if let Some(mut h) = self.map.get_mut(id) {
            if let HealthState::Cooling { until, .. } = h.state {
                if Instant::now() >= until {
                    h.state = HealthState::HalfOpen;
                    drop(h);
                    dec_counter(&self.cooling_counts, id.provider());
                    return HealthState::HalfOpen;
                }
            } else {
                return h.state;
            }
        }
        // Not present, or was still Cooling (no transition): read current state.
        self.map
            .get(id)
            .map(|h| h.state)
            .unwrap_or(HealthState::Healthy)
    }

    /// `is_healthy` auto-advances the cooling->half-open state machine (see
    /// `health_state`), so callers never need a separate background probe task
    /// to recover idle tiers.
    pub fn is_healthy(&self, id: &EndpointId) -> bool {
        matches!(self.health_state(id), HealthState::Healthy | HealthState::HalfOpen)
    }

    pub fn is_half_open(&self, id: &EndpointId) -> bool {
        matches!(self.map.get(id).map(|h| h.state), Some(HealthState::HalfOpen))
    }

    pub fn mark_success(&self, id: &EndpointId) {
        self.register(id);
        let was_cooling = if let Some(mut h) = self.map.get_mut(id) {
            let was = matches!(h.state, HealthState::Cooling { .. });
            h.state = HealthState::Healthy;
            h.consecutive_failures = 0;
            h.last_failure = None;
            was
        } else { false };
        if was_cooling {
            dec_counter(&self.cooling_counts, id.provider());
        }
    }

    pub fn mark_failure(&self, id: &EndpointId) {
        self.register(id);
        let mut entry = self.map.entry(id.clone()).or_default();
        let was_cooling = matches!(entry.state, HealthState::Cooling { .. });
        let was_half_open = matches!(entry.state, HealthState::HalfOpen);
        entry.consecutive_failures += 1;
        entry.last_failure = Some(Instant::now());
        if entry.consecutive_failures >= 3 || was_half_open {
            let backoff = if was_half_open {
                Duration::from_secs(30)
            } else {
                let exp = (entry.consecutive_failures.saturating_sub(3)).min(4) as u32;
                Duration::from_secs(30 * 2u64.pow(exp))
            };
            // Jitter the cooling window by ±10–20% so a fleet of endpoints that
            // fail together don't all re-probe in lockstep (avoids thundering
            // herd on recovery). Multiplier in [0.9, 1.2).
            let jitter = rand::rng().random_range(0.9..1.2);
            let backoff = backoff.mul_f64(jitter);
            let capped = std::cmp::min(backoff, Duration::from_secs(300));
            entry.state = HealthState::Cooling { until: Instant::now() + capped, failures: entry.consecutive_failures };
        }
        let now_cooling = matches!(entry.state, HealthState::Cooling { .. });
        if now_cooling && !was_cooling {
            inc_counter(&self.cooling_counts, id.provider());
        }
    }

    /// Transition a HalfOpen endpoint into a short "probing" cooling window so
    /// that concurrent requests don't all treat it as a live probe. The single
    /// in-flight probe still proceeds; its result drives the final transition.
    pub fn mark_probing(&self, id: &EndpointId) {
        if let Some(mut h) = self.map.get_mut(id) {
            if let HealthState::HalfOpen = h.state {
                let failures = h.consecutive_failures;
                h.state = HealthState::Cooling { until: Instant::now() + Duration::from_secs(5), failures };
                drop(h);
                inc_counter(&self.cooling_counts, id.provider());
            }
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

    /// Snapshot the health state of a batch of endpoints in a single pass (one
    /// DashMap lock per id, with the cooling->half-open transition folded in).
    /// Used by `candidates_ordered` so a 40-entry tier does not take 80+ locks.
    pub fn snapshot_states(&self, ids: &[EndpointId]) -> HashMap<EndpointId, HealthState> {
        let mut out = HashMap::with_capacity(ids.len());
        for id in ids {
            out.insert(id.clone(), self.health_state(id));
        }
        out
    }

    /// Transition cooling -> half-open if timer expired
    pub fn maybe_half_open(&self, id: &EndpointId) {
        if let Some(mut h) = self.map.get_mut(id) {
            if let HealthState::Cooling { until, .. } = h.state {
                if Instant::now() >= until {
                    h.state = HealthState::HalfOpen;
                    drop(h);
                    dec_counter(&self.cooling_counts, id.provider());
                }
            }
        }
    }

    /// Check if provider is degraded: >70% of its endpoints cooling.
    /// O(1): reads two atomic counters, no map scan.
    pub fn is_provider_degraded(&self, provider: &str) -> bool {
        let total = self.total_counts.get(provider).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0);
        if total == 0 { return false; }
        let cooling = self.cooling_counts.get(provider).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0);
        (cooling as f64) / (total as f64) > 0.7
    }

    /// Parse Retry-After header value (seconds or http-date fallback)
    pub fn apply_retry_after(&self, id: &EndpointId, retry_after: Option<&str>) {
        if let Some(val) = retry_after {
            if let Ok(secs) = val.parse::<u64>() {
                self.register(id);
                let was_cooling = self.map.get(id).map(|h| matches!(h.state, HealthState::Cooling { .. })).unwrap_or(false);
                let until = Instant::now() + Duration::from_secs(secs);
                self.map.insert(id.clone(), EndpointHealth { state: HealthState::Cooling { until, failures: 1 }, consecutive_failures: 1, last_failure: Some(Instant::now()) });
                if !was_cooling {
                    inc_counter(&self.cooling_counts, id.provider());
                }
            }
        }
    }
}

impl Default for HealthRegistry {
    fn default() -> Self { Self::new() }
}

// Per-tier smooth-WRR state, keyed by `EndpointId` (not by candidate position)
// so that reordering of `candidates_ordered` output does not misalign weights.
struct WrrState {
    // Number of candidates the last time we (re)built the weight map. Used to
    // detect topology changes and reset stale weights.
    len: usize,
    // Current (smooth) weight per endpoint id.
    weights: HashMap<EndpointId, i64>,
}

// Balancer combines KeyRing per provider + health
pub struct Balancer {
    pub keyrings: DashMap<String, KeyRing>,
    pub health: HealthRegistry,
    // Quota-banned keys: "provider:key" -> ban expiry. A key banned for quota
    // is skipped by `next_key` until its ban expires (auto-cleared on access).
    banned_keys: DashMap<String, Instant>,
    // Per-tier smooth-WRR current-weight state (indexed by tier name, then by
    // `EndpointId` so reordering candidates never misaligns weights).
    wrr: DashMap<String, WrrState>,
}

impl Balancer {
    pub fn new() -> Self {
        Self {
            keyrings: DashMap::new(),
            health: HealthRegistry::new(),
            banned_keys: DashMap::new(),
            wrr: DashMap::new(),
        }
    }

    /// Ban a key for `secs` seconds due to a quota error. The ban key is
    /// scoped as `"provider:key"` so it only affects this provider.
    pub fn ban_key(&self, provider: &str, key: &str, secs: u64) {
        let k = format!("{}:{}", provider, key);
        self.banned_keys.insert(k, Instant::now() + Duration::from_secs(secs));
        tracing::warn!(provider = provider, key = key, secs = secs, "key banned for quota");
    }

    /// Returns true if the key is currently quota-banned for this provider.
    /// Expired bans are auto-cleared (and report as not banned).
    pub fn is_key_banned(&self, provider: &str, key: &str) -> bool {
        let k = format!("{}:{}", provider, key);
        if let Some(expiry) = self.banned_keys.get(&k) {
            if Instant::now() >= *expiry {
                drop(expiry);
                self.banned_keys.remove(&k);
                false
            } else {
                true
            }
        } else {
            false
        }
    }

    pub fn update_keys(&self, provider: &str, keys: Vec<ApiKey>) {
        self.keyrings.insert(provider.to_string(), KeyRing::new(keys));
    }

    pub fn next_key(&self, provider: &str) -> Option<(ApiKey, usize)> {
        let ring = self.keyrings.get(provider)?;
        let len = ring.len();
        if len == 0 {
            return None;
        }
        // Peek-then-advance: we only advance the cursor when we actually return a
        // key. This preserves round-robin fairness among *healthy* keys and,
        // crucially, avoids cursor drift when every key is dead/banned (the old
        // code advanced the cursor on every skipped key, starving fairness and
        // walking the cursor far ahead on fully-dead providers).
        let start = ring.cursor.load(Ordering::Relaxed);
        for step in 0..len {
            let idx = (start + step) % len;
            let k = &ring.keys[idx];
            if self.health.is_key_dead(k.expose()) {
                continue;
            }
            if self.is_key_banned(provider, k.expose()) {
                continue;
            }
            // Claim this slot so the next call continues after it.
            ring.cursor.store((start + step + 1) % len, Ordering::Relaxed);
            return Some((k.clone(), idx));
        }
        // All keys dead/banned: leave the cursor untouched (no drift).
        None
    }

    /// Number of configured keys for a provider (0 if unknown).
    pub fn keys_len(&self, provider: &str) -> usize {
        self.keyrings.get(provider).map(|r| r.len()).unwrap_or(0)
    }

    pub fn candidates_ordered<'a>(&self, tier: &'a xrouter_core::Tier) -> Vec<&'a ModelEntry> {
        // Snapshot health once per tier (single pass, one lock per endpoint) so a
        // large tier doesn't take 40+ DashMap locks on the hot path.
        let ids: Vec<EndpointId> = tier.entries.iter().map(|e| e.endpoint_id.clone()).collect();
        let states = self.health.snapshot_states(&ids);
        // Provider degraded check: if provider degraded, deprioritize its entries but keep within tier.
        // `strict` tiers honor provider-degradation signals; non-strict tiers treat all entries equally.
        let honor_degradation = tier.strict;
        let mut healthy: Vec<(usize, &ModelEntry)> = Vec::new();
        let mut degraded: Vec<(usize, &ModelEntry)> = Vec::new();
        let mut unhealthy: Vec<(usize, &ModelEntry)> = Vec::new();
        for (i, e) in tier.entries.iter().enumerate() {
            let st = states.get(&e.endpoint_id).copied().unwrap_or(HealthState::Healthy);
            let is_healthy = matches!(st, HealthState::Healthy | HealthState::HalfOpen);
            let is_degraded_provider = honor_degradation && self.health.is_provider_degraded(&e.provider);
            if is_healthy && !is_degraded_provider {
                healthy.push((i, e));
            } else if is_healthy && is_degraded_provider {
                degraded.push((i, e));
            } else {
                unhealthy.push((i, e));
            }
        }
        // Prefer the tier's declared default entry first within each group.
        if let Some(pos) = healthy.iter().position(|(i, _)| *i == tier.default_entry) {
            healthy.rotate_left(pos);
        }
        if let Some(pos) = degraded.iter().position(|(i, _)| *i == tier.default_entry) {
            degraded.rotate_left(pos);
        }
        // Higher-weight entries are preferred (tried first).
        healthy.sort_by(|a, b| b.1.weight.cmp(&a.1.weight));
        degraded.sort_by(|a, b| b.1.weight.cmp(&a.1.weight));
        let mut res: Vec<&ModelEntry> = healthy.into_iter().map(|(_, e)| e).collect();
        res.extend(degraded.into_iter().map(|(_, e)| e));
        res.extend(unhealthy.into_iter().map(|(_, e)| e));
        res
    }

    /// Smooth weighted round-robin (Nginx-style) selection among the tier's
    /// candidates. State is kept per tier in `self.wrr`, keyed by `EndpointId`
    /// (not candidate position) so that reordering of `candidates_ordered`
    /// output never misaligns weights. The stored weight map is reset when the
    /// candidate count changes (topology change).
    pub fn pick_weighted<'a>(&self, tier: &'a xrouter_core::Tier) -> Option<&'a ModelEntry> {
        let candidates = self.candidates_ordered(tier);
        if candidates.is_empty() { return None; }
        // If all weights 1, just return first (ordered).
        let all_one = candidates.iter().all(|c| c.weight == 1);
        if all_one { return Some(candidates[0]); }
        let key = tier.name.clone();
        let total: u32 = candidates.iter().map(|c| c.weight).sum();
        let mut state = self.wrr.entry(key).or_insert_with(|| WrrState {
            len: candidates.len(),
            weights: HashMap::new(),
        });
        // Topology changed (candidate count differs) → reset weights.
        if state.len != candidates.len() {
            state.len = candidates.len();
            state.weights.clear();
        }
        let mut best = 0usize;
        let mut best_val = i64::MIN;
        for (i, c) in candidates.iter().enumerate() {
            let cur = state.weights.entry(c.endpoint_id.clone()).or_insert(0);
            *cur += c.weight as i64;
            if *cur > best_val { best_val = *cur; best = i; }
        }
        if let Some(cur) = state.weights.get_mut(&candidates[best].endpoint_id) {
            *cur -= total as i64;
        }
        Some(candidates[best])
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
