use serde::{Deserialize, Serialize};
use crate::RouterError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelEntry {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 { 1 }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tier {
    pub name: String,
    #[serde(default = "default_true")]
    pub strict: bool,
    #[serde(default)]
    pub default_entry: usize,
    pub entries: Vec<ModelEntry>,
}

fn default_true() -> bool { true }

impl Tier {
    pub fn new(name: impl Into<String>, entries: Vec<ModelEntry>) -> Self {
        Self { name: name.into(), strict: true, default_entry: 0, entries }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TierRegistry {
    tiers: Vec<Tier>,
}

impl TierRegistry {
    pub fn new(tiers: Vec<Tier>) -> Self { Self { tiers } }
    pub fn tiers(&self) -> &[Tier] { &self.tiers }
    pub fn get(&self, name: &str) -> Option<&Tier> {
        self.tiers.iter().find(|t| t.name == name)
    }
    pub fn names(&self) -> Vec<String> {
        self.tiers.iter().map(|t| t.name.clone()).collect()
    }
    /// Strict resolution: return tier if exists else error
    pub fn resolve(&self, alias: &str) -> Result<&Tier, RouterError> {
        self.get(alias).ok_or_else(|| RouterError::UnknownTier {
            tier: alias.to_string(),
            available: self.names(),
        })
    }
    pub fn add_or_replace(&mut self, tier: Tier) {
        if let Some(pos) = self.tiers.iter().position(|t| t.name == tier.name) {
            self.tiers[pos] = tier;
        } else {
            self.tiers.push(tier);
        }
    }
    pub fn remove(&mut self, name: &str) -> Option<Tier> {
        if let Some(pos) = self.tiers.iter().position(|t| t.name == name) {
            Some(self.tiers.remove(pos))
        } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_resolution() {
        let reg = TierRegistry::new(vec![
            Tier::new("fast", vec![ModelEntry { provider: "opencode-zen".into(), model: "big-pickle".into(), is_default: true, weight: 1 }])
        ]);
        assert!(reg.resolve("fast").is_ok());
        assert!(reg.resolve("slow").is_err());
        // ensure no fuzzy
        assert!(reg.resolve("Fast").is_err());
    }
    #[test]
    fn add_replace() {
        let mut reg = TierRegistry::new(vec![]);
        reg.add_or_replace(Tier::new("a", vec![]));
        reg.add_or_replace(Tier::new("a", vec![ModelEntry { provider: "p".into(), model: "m".into(), is_default: false, weight: 1 }]));
        assert_eq!(reg.tiers().len(), 1);
        assert_eq!(reg.get("a").unwrap().entries.len(), 1);
    }
}
