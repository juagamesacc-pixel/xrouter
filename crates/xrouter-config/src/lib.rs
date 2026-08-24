pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::{Context, Result};
use xrouter_core::{Tier, ModelEntry};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    #[serde(default)]
    pub default_tier: Option<String>,
    #[serde(default)]
    pub api_token: Option<String>,
}

impl Default for Settings {
    fn default() -> Self { Self { default_tier: Some("fast".into()), api_token: None } }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    #[serde(default = "default_kind")]
    pub kind: String, // "openai-compat" | "anthropic"
    pub base_url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub rr_cursor: usize,
}

fn default_kind() -> String { "openai-compat".into() }
fn default_true() -> bool { true }

impl ProviderConfig {
    pub fn protocol(&self) -> xrouter_core::Protocol {
        match self.kind.as_str() {
            "anthropic" => xrouter_core::Protocol::Anthropic,
            _ => xrouter_core::Protocol::OpenAiCompat,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub tiers: Vec<Tier>,
}

impl Config {
    pub fn default_with_builtins() -> Self {
        let mut providers = HashMap::new();
        providers.insert("opencode-zen".to_string(), ProviderConfig {
            kind: "openai-compat".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            enabled: true,
            keys: vec![],
            rr_cursor: 0,
        });
        providers.insert("openrouter".to_string(), ProviderConfig {
            kind: "openai-compat".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            enabled: true,
            keys: vec![],
            rr_cursor: 0,
        });
        let tiers = vec![Tier {
            name: "big-pickle".into(),
            strict: true,
            default_entry: 0,
            entries: vec![ModelEntry { provider: "opencode-zen".into(), model: "big-pickle".into(), is_default: true, weight: 1 }],
        }];
        Self { settings: Settings { default_tier: Some("big-pickle".into()), api_token: None }, providers, tiers }
    }
    pub fn tier_registry(&self) -> xrouter_core::tier::TierRegistry {
        xrouter_core::tier::TierRegistry::new(self.tiers.clone())
    }
}

pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/xrouter/config.toml")
}

pub fn cache_models_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/xrouter/models.json")
}

pub fn load() -> Result<Config> {
    let p = config_path();
    load_from(&p)
}

pub fn load_from(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default_with_builtins());
    }
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&s).context("parse config toml")?;
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    save_to(cfg, &config_path())
}

pub fn save_to(cfg: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create config dir")?;
    }
    let toml_str = toml::to_string_pretty(cfg).context("serialize config")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml_str).context("write tmp")?;
    // chmod 0600 on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&tmp, perm);
    }
    std::fs::rename(&tmp, path).context("rename config")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perm);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let cfg = Config::default_with_builtins();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }
    #[test]
    fn save_load_tmp() {
        let dir = std::env::temp_dir().join(format!("xrouter-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let cfg = Config::default_with_builtins();
        save_to(&cfg, &path).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(cfg, loaded);
        let _ = std::fs::remove_dir_all(dir);
    }
}
