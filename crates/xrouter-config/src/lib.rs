pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::{Context, Result};
use xrouter_core::{Tier, ModelEntry, EndpointId};
pub use xrouter_auth::DeviceAccountConfig;

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
    pub kind: String, // "openai-compat" | "anthropic" | "kiro" | "antigravity" | "device:<name>"
    pub base_url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub keys: Vec<String>,
    /// Seconds a key is banned after a quota (429) error. Default 300.
    #[serde(default = "default_quota_ban_secs")]
    pub quota_ban_secs: u64,
    /// Device-login account metadata (no secrets). Used when `kind` is a device
    /// provider. Secrets live in the off-RAM device-account token store.
    #[serde(default)]
    pub accounts: Vec<DeviceAccountConfig>,
}

fn default_kind() -> String { "openai-compat".into() }
fn default_true() -> bool { true }
fn default_quota_ban_secs() -> u64 { 300 }

impl ProviderConfig {
    pub fn protocol(&self) -> xrouter_core::Protocol {
        match self.kind.as_str() {
            "anthropic" => xrouter_core::Protocol::Anthropic,
            _ => xrouter_core::Protocol::OpenAiCompat,
        }
    }

    /// True when this provider authenticates via the device-login flow (no API
    /// keys). Matches `kiro`, `antigravity`, and `device:<name>` kinds.
    pub fn is_device(&self) -> bool {
        xrouter_auth::is_device_kind(&self.kind)
    }

    /// Bare provider name for a device provider (`device:kiro` -> `kiro`).
    pub fn device_provider_name(&self) -> &str {
        xrouter_auth::normalize_provider(&self.kind)
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
            quota_ban_secs: default_quota_ban_secs(),
            accounts: vec![],
        });
        providers.insert("openrouter".to_string(), ProviderConfig {
            kind: "openai-compat".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            enabled: true,
            keys: vec![],
            quota_ban_secs: default_quota_ban_secs(),
            accounts: vec![],
        });
        // Built-in free image provider (Together AI). OpenAI-compatible image
        // endpoint lives at {base_url}/images/generations.
        // NOTE: Together's documented host is https://api.together.xyz/v1, but
        // the librarian confirmed https://api.together.ai/v1 works for
        // /images/generations as well. We use the .ai host so the path suffix
        // appends cleanly; the .xyz host is equivalent.
        providers.insert("together-image".to_string(), ProviderConfig {
            kind: "openai-compat".into(),
            base_url: "https://api.together.ai/v1".into(),
            enabled: true,
            keys: vec![],
            quota_ban_secs: default_quota_ban_secs(),
            accounts: vec![],
        });
        let tiers = vec![
            Tier {
                name: "big-pickle".into(),
                strict: true,
                default_entry: 0,
                entries: vec![ModelEntry { provider: "opencode-zen".into(), model: "big-pickle".into(), is_default: true, weight: 1, endpoint_id: EndpointId::new("opencode-zen", "big-pickle") }],
            },
            // Built-in image tier backed by the free Together AI image provider.
            Tier {
                name: "images".into(),
                strict: true,
                default_entry: 0,
                entries: vec![ModelEntry { provider: "together-image".into(), model: "black-forest-labs/FLUX.1-schnell".into(), is_default: true, weight: 1, endpoint_id: EndpointId::new("together-image", "black-forest-labs/FLUX.1-schnell") }],
            },
        ];
        let mut cfg = Self { settings: Settings { default_tier: Some("big-pickle".into()), api_token: None }, providers, tiers };
        cfg.finalize();
        cfg
    }

    /// Assign a stable `endpoint_id` to every `ModelEntry` from its
    /// `(provider, model)` pair. Called once at config load so the balancer
    /// can avoid recomputing ids on the hot path.
    pub fn finalize(&mut self) {
        for tier in &mut self.tiers {
            for e in &mut tier.entries {
                e.endpoint_id = EndpointId::new(&e.provider, &e.model);
            }
        }
    }
    pub fn tier_registry(&self) -> xrouter_core::tier::TierRegistry {
        xrouter_core::tier::TierRegistry::new(self.tiers.clone())
    }
}

pub fn config_path() -> PathBuf {
    // Allow overriding the config location (used by tests and the CLI's
    // `auth` subcommands so they can operate on a temp config without
    // touching the real HOME). The wizard server honors the same variable.
    if let Ok(p) = std::env::var("XROUTER_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
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
    let mut cfg: Config = toml::from_str(&s).context("parse config toml")?;
    cfg.finalize();
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
    // Restrict the on-disk config (which may carry the router `api_token`) to
    // owner-only access. The 0600 chmod is unix-only: on Windows the
    // `#[cfg(unix)]` gate skips it and we instead rely on the OS file ACLs /
    // the fact that the file lives under the user's profile directory. There is
    // no portable equivalent of 0600 in the standard library, so Windows
    // hardening is delegated to the filesystem ACLs set by the OS.
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

/// Generate a secure router API key: `xr_` + 32 hex chars (16 random bytes).
///
/// This is the single canonical implementation; the wizard and CLI historically
/// carried duplicate copies, but new code should call this instead of
/// re-implementing the format.
pub fn gen_router_key() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let n: u128 = rng.random();
    format!("xr_{:032x}", n)
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
