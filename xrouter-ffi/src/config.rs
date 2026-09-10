//! Configuration management for FFI boundary

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use xrouter_config::Config;

/// Server binding configuration (persisted separately from xrouter Config)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 3001,
        }
    }
}

/// Get the default config directory for the app
pub fn default_config_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp"))
        .join("com.xrouter.app")
}

/// Get the default xrouter config file path
pub fn default_config_path() -> PathBuf {
    default_config_dir().join("xrouter.toml")
}

/// Get the server config file path (FFI-specific)
pub fn server_config_path() -> PathBuf {
    default_config_dir().join("server.json")
}

/// Load server binding config
pub fn load_server_config() -> ServerConfig {
    let path = server_config_path();
    if path.exists() {
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str(&s) {
                return cfg;
            }
        }
    }
    ServerConfig::default()
}

/// Save server binding config
pub fn save_server_config(cfg: &ServerConfig) -> Result<()> {
    let path = server_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Load config from file, creating default if it doesn't exist.
/// Also sets XROUTER_CONFIG env var so xrouter_config::load() (used by AppState::reload())
/// reads from the same path.
pub fn load_or_create_config(path: Option<&str>) -> Result<Config> {
    let config_path = match path {
        Some(p) => PathBuf::from(p),
        None => default_config_path(),
    };

    // Set the env var so xrouter_config::load() and AppState::reload() use our path
    std::env::set_var("XROUTER_CONFIG", &config_path);

    if config_path.exists() {
        tracing::info!("Loading config from: {:?}", config_path);
        Config::load_from(&config_path)
            .with_context(|| format!("Failed to load config from {:?}", config_path))
    } else {
        tracing::info!("No config found, creating default at: {:?}", config_path);
        create_default_config(&config_path)?;
        Config::load_from(&config_path)
            .with_context(|| format!("Failed to load newly created config at {:?}", config_path))
    }
}

/// Create a sensible default configuration
fn create_default_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let default_toml = r#"# xrouter configuration
# Edit this file or use the app's Settings UI

[tiers.openai]
provider = "openai"
api_keys = []
rate_limit = 60
daily_limit = 1000
timeout_secs = 120
max_retries = 3

[tiers.anthropic]
provider = "anthropic"
api_keys = []
rate_limit = 60
daily_limit = 1000
timeout_secs = 120
max_retries = 3

[tiers.gemini]
provider = "gemini"
api_keys = []
rate_limit = 60
daily_limit = 1000
timeout_secs = 120
max_retries = 3

[defaults]
tier = "openai"
fallback_tiers = ["anthropic", "gemini"]
health_check_interval_secs = 300
"#;

    std::fs::write(path, default_toml)?;
    tracing::info!("Created default config at {:?}", path);
    Ok(())
}

/// Save config to file
pub fn save_config(config: &Config, path: Option<&str>) -> Result<()> {
    let config_path = match path {
        Some(p) => PathBuf::from(p),
        None => default_config_path(),
    };

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let toml_string = toml::to_string_pretty(config)
        .context("Failed to serialize config")?;

    std::fs::write(&config_path, toml_string)?;
    tracing::info!("Config saved to {:?}", config_path);
    Ok(())
}

/// Export current config as JSON string (for FFI consumption)
pub fn config_to_json(config: &Config) -> Result<String> {
    let json = serde_json::to_string_pretty(config)?;
    Ok(json)
}

/// Get config as raw TOML string
pub fn config_to_toml(config: &Config) -> Result<String> {
    let toml = toml::to_string_pretty(config)?;
    Ok(toml)
}

/// Save config from JSON value (parse as Config, write as TOML)
pub fn save_config_json(json: &serde_json::Value) -> Result<()> {
    let config_path = default_config_path();

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Parse the JSON directly into a Config struct, then serialize to TOML.
    // This preserves all structure and handles unknown fields gracefully.
    let mut cfg: Config = serde_json::from_value(json.clone())
        .context("Failed to parse JSON into Config")?;
    cfg.finalize();

    let toml_string = toml::to_string_pretty(&cfg)
        .context("Failed to serialize config to TOML")?;

    std::fs::write(&config_path, toml_string)?;
    tracing::info!("Config saved from JSON to {:?}", config_path);
    Ok(())
}
