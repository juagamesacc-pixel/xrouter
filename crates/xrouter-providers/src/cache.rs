use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::RawModel;

const TTL: Duration = Duration::from_secs(600); // 10 min

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub models: Vec<RawModel>,
    pub fetched_at: u64, // unix secs
}

impl CacheEntry {
    fn is_fresh(&self) -> bool {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        now.saturating_sub(self.fetched_at) < TTL.as_secs()
    }
}

/// In-memory + persisted model cache per provider.
#[derive(Debug, Clone)]
pub struct ModelCache {
    map: Arc<DashMap<String, (Vec<RawModel>, Instant)>>,
    persist_path: PathBuf,
}

impl ModelCache {
    pub fn new(persist_path: PathBuf) -> Self {
        let cache = Self { map: Arc::new(DashMap::new()), persist_path };
        // try load persisted snapshot
        cache.load_snapshot();
        cache
    }

    pub fn get(&self, provider: &str) -> Option<Vec<RawModel>> {
        if let Some(entry) = self.map.get(provider) {
            let (models, at) = entry.value();
            if at.elapsed() < TTL {
                return Some(models.clone());
            }
        }
        None
    }

    pub fn is_fresh(&self, provider: &str) -> bool {
        if let Some(entry) = self.map.get(provider) {
            return entry.value().1.elapsed() < TTL;
        }
        false
    }

    pub fn insert(&self, provider: String, models: Vec<RawModel>) {
        self.map.insert(provider, (models.clone(), Instant::now()));
        let _ = self.save_snapshot();
    }

    pub fn get_or_stale(&self, provider: &str) -> Option<Vec<RawModel>> {
        self.map.get(provider).map(|e| e.value().0.clone())
    }

    fn cache_dir(&self) -> Option<PathBuf> {
        self.persist_path.parent().map(|p| p.to_path_buf())
    }

    pub fn load_snapshot(&self) {
        if !self.persist_path.exists() { return; }
        if let Ok(data) = std::fs::read_to_string(&self.persist_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, CacheEntry>>(&data) {
                let now = Instant::now();
                for (k, v) in map {
                    // treat persisted as stale but usable; set Instant to now - delta if fresh else old
                    let age_secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs().saturating_sub(v.fetched_at);
                    let instant = if age_secs < TTL.as_secs() { now - Duration::from_secs(age_secs) } else { now - TTL - Duration::from_secs(1) };
                    self.map.insert(k, (v.models, instant));
                }
            }
        }
    }

    pub fn save_snapshot(&self) -> anyhow::Result<()> {
        if let Some(dir) = self.cache_dir() {
            let _ = std::fs::create_dir_all(&dir);
        }
        let mut out: HashMap<String, CacheEntry> = HashMap::new();
        let now_secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        for entry in self.map.iter() {
            out.insert(entry.key().clone(), CacheEntry { models: entry.value().0.clone(), fetched_at: now_secs });
        }
        let json = serde_json::to_string_pretty(&out)?;
        let tmp = self.persist_path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.persist_path)?;
        Ok(())
    }

    pub fn snapshot_json(&self) -> serde_json::Value {
        let mut out = serde_json::json!({});
        for entry in self.map.iter() {
            let fresh = entry.value().1.elapsed() < TTL;
            out[entry.key()] = serde_json::json!({"count": entry.value().0.len(), "fresh": fresh});
        }
        out
    }
}

impl Default for ModelCache {
    fn default() -> Self {
        let path = xrouter_config::cache_models_path();
        Self::new(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_freshness() {
        let dir = std::env::temp_dir().join(format!("xrouter-cache-test-{}", std::process::id()));
        let path = dir.join("models.json");
        let c = ModelCache::new(path.clone());
        c.insert("p1".to_string(), vec![RawModel { id: "m1".into(), name: None }]);
        assert!(c.is_fresh("p1"));
        assert!(c.get("p1").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
