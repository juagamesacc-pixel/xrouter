use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod free;
pub mod tier;

pub use free::is_free;
pub use tier::{ModelEntry, Tier, TierRegistry};

/// Provider identifier, e.g. "opencode-zen"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

impl From<String> for ProviderId {
    fn from(s: String) -> Self { Self(s) }
}
impl From<&str> for ProviderId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}
impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
}

/// API key wrapper with debug masking
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey(pub String);

impl ApiKey {
    pub fn masked(&self) -> String {
        if self.0.len() <= 4 {
            "****".to_string()
        } else {
            format!("sk-…{}", &self.0[self.0.len().saturating_sub(4)..])
        }
    }
    pub fn expose(&self) -> &str { &self.0 }
}

impl From<String> for ApiKey {
    fn from(s: String) -> Self { Self(s) }
}
impl From<&str> for ApiKey {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

/// Protocol kind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    #[serde(rename = "openai-compat")]
    OpenAiCompat,
    Anthropic,
}

/// Errors in routing
#[derive(Debug, Error)]
pub enum RouterError {
    #[error("unknown tier: {tier}")]
    UnknownTier { tier: String, available: Vec<String> },
    #[error("tier exhausted: {tier}")]
    TierExhausted { tier: String },
    #[error("no valid keys")]
    NoValidKeys,
    #[error("upstream error: {status}")]
    UpstreamError { status: u16, body: String },
    #[error("timeout")]
    Timeout,
    #[error("config error: {0}")]
    Config(String),
}

pub fn tier_exhausted_body(tier: &str) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "type": "tier_exhausted",
            "tier": tier,
            "message": format!("all models/providers for tier '{}' failed", tier)
        }
    })
}

pub fn unknown_tier_body(tier: &str, available: &[String]) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "type": "unknown_tier",
            "tier": tier,
            "available": available,
            "message": format!("unknown tier '{}'", tier)
        }
    })
}
