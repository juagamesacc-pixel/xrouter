pub mod cache;
pub use cache::ModelCache;

use serde::{Deserialize, Serialize};
use async_trait::async_trait;
use reqwest::Client;
use xrouter_core::ApiKey;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawModel {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RequestCtx {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key: ApiKey,
    pub body: serde_json::Value, // already translated or raw
    pub stream: bool,
}

pub struct UpstreamResponse {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub is_stream: bool,
    /// Raw upstream response. Consume via `response.bytes()` (buffered) or
    /// `response.bytes_stream()` (real streaming passthrough) in the server.
    pub response: reqwest::Response,
}

impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("status", &self.status)
            .field("is_stream", &self.is_stream)
            .finish()
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn list_models(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>>;
    async fn send(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse>;
    /// Image generation. Posts `ctx.body` to `{base_url}/images/generations`.
    /// Providers without an image endpoint (e.g. Anthropic) return an error.
    async fn send_images(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse> {
        let _ = ctx;
        anyhow::bail!("provider does not support image generation")
    }
    fn protocol(&self) -> xrouter_core::Protocol;
    fn base_url(&self) -> &str;
}

pub struct OpenAiCompatAdapter {
    pub base_url: String,
    pub client: Client,
}

impl OpenAiCompatAdapter {
    pub fn new(base_url: String, client: Client) -> Self { Self { base_url, client } }
}

#[async_trait]
impl Provider for OpenAiCompatAdapter {
    async fn list_models(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let resp = self.client.get(&url)
            .header("Authorization", format!("Bearer {}", key.expose()))
            .send().await?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
            anyhow::bail!("list_models {}: {}", status, txt);
        }
        let v: serde_json::Value = resp.json().await?;
        // OpenAI format: { "data": [ { "id": "..."} ] } or { "models": [...] } or array directly
        let mut out = Vec::new();
        if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
            for item in arr {
                if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                    out.push(RawModel { id: id.to_string(), name: item.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()) });
                }
            }
        } else if let Some(arr) = v.get("models").and_then(|d| d.as_array()) {
            for item in arr {
                if let Some(id) = item.get("id").and_then(|x| x.as_str()).or_else(|| item.get("name").and_then(|x| x.as_str())) {
                    out.push(RawModel { id: id.to_string(), name: None });
                }
            }
        } else if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                    out.push(RawModel { id: id.to_string(), name: None });
                } else if let Some(s) = item.as_str() {
                    out.push(RawModel { id: s.to_string(), name: None });
                }
            }
        } else {
            // fallback: try to parse as {"data": [{"id":...}]} already handled; if empty return empty
        }
        Ok(out)
    }

    async fn send(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut req = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", ctx.api_key.expose()))
            .header("Content-Type", "application/json")
            .json(&ctx.body);
        // Signal SSE support when the caller wants a streamed response so
        // upstreams return `text/event-stream` instead of buffering.
        if ctx.stream {
            req = req.header("Accept", "text/event-stream");
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let is_stream = ctx.stream;
        // Hand the raw response to the caller. The server decides whether to
        // buffer (`bytes()`) or stream passthrough (`bytes_stream()`).
        Ok(UpstreamResponse { status, headers, is_stream, response: resp })
    }

    async fn send_images(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse> {
        let url = format!("{}/images/generations", self.base_url.trim_end_matches('/'));
        let resp = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", ctx.api_key.expose()))
            .header("Content-Type", "application/json")
            .json(&ctx.body)
            .send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        // Image responses are never streamed in this router.
        Ok(UpstreamResponse { status, headers, is_stream: false, response: resp })
    }

    fn protocol(&self) -> xrouter_core::Protocol { xrouter_core::Protocol::OpenAiCompat }
    fn base_url(&self) -> &str { &self.base_url }
}

pub struct AnthropicAdapter {
    pub base_url: String,
    pub client: Client,
}

impl AnthropicAdapter {
    pub fn new(base_url: String, client: Client) -> Self { Self { base_url, client } }
}

#[async_trait]
impl Provider for AnthropicAdapter {
    async fn list_models(&self, _key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        // Anthropic does not have list models endpoint; return empty
        Ok(vec![])
    }
    async fn send(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = self.client.post(&url)
            .header("x-api-key", ctx.api_key.expose())
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&ctx.body)
            .send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        Ok(UpstreamResponse { status, headers, is_stream: ctx.stream, response: resp })
    }
    fn protocol(&self) -> xrouter_core::Protocol { xrouter_core::Protocol::Anthropic }
    fn base_url(&self) -> &str { &self.base_url }
}

/// Provider that authenticates via the device-login flow. The `api_key` field
/// of `RequestCtx` carries the (already refreshed) device access token, which
/// is sent as a `Bearer` credential. Some device providers (e.g. antigravity)
/// also require an extra static header (e.g. `X-Goog-*`), supplied here.
///
/// `kind` is the normalized provider name (`kiro`, `antigravity`, or a generic
/// `device:<name>` bare name). It selects the model-discovery strategy used by
/// [`DeviceProvider::list_models`]. `profile_arn` is an optional CodeWhisperer
/// profile ARN used by kiro to derive its runtime AWS region; it is `None` when
/// not available (e.g. when built via [`make_provider`]), in which case kiro
/// falls back to `us-east-1`.
pub struct DeviceProvider {
    pub base_url: String,
    pub client: Client,
    pub extra_headers: reqwest::header::HeaderMap,
    pub kind: String,
    pub profile_arn: Option<String>,
}

impl DeviceProvider {
    pub fn new(
        base_url: String,
        client: Client,
        extra_headers: reqwest::header::HeaderMap,
        kind: String,
        profile_arn: Option<String>,
    ) -> Self {
        Self { base_url, client, extra_headers, kind, profile_arn }
    }

    /// kiro model discovery against the AWS Q / CodeWhisperer
    /// `ListAvailableModels` endpoint.
    async fn list_models_kiro(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        let primary_region = kiro_region_from_arn(self.profile_arn.as_deref());
        // Try the derived region first, then fall back to us-east-1 once.
        let regions: Vec<String> = if primary_region == "us-east-1" {
            vec!["us-east-1".to_string()]
        } else {
            vec![primary_region, "us-east-1".to_string()]
        };
        let mut last_err = String::new();
        for region in &regions {
            let base = kiro_q_base_url(region);
            let url = format!("{}/ListAvailableModels?origin=AI_EDITOR", base.trim_end_matches('/'));
            // Bound each attempt so a hung/unreachable AWS endpoint can't
            // wedge the caller (wizard/server) indefinitely.
            match tokio::time::timeout(
                std::time::Duration::from_secs(20),
                kiro_fetch_models(&self.client, &url, key),
            )
            .await
            {
                Ok(Ok(models)) => return Ok(models),
                Ok(Err(e)) => {
                    last_err = e.to_string();
                    warn!("kiro ListAvailableModels attempt failed for region {region}: {e}");
                }
                Err(_) => {
                    last_err = "timed out after 20s".to_string();
                    warn!("kiro ListAvailableModels attempt timed out for region {region}");
                }
            }
        }
        warn!(
            "kiro list_models failed ({}); serving static fallback catalog",
            last_err
        );
        Ok(KIRO_FALLBACK
            .iter()
            .map(|id| RawModel { id: id.to_string(), name: None })
            .collect())
    }

    /// antigravity model discovery against the Google Cloud Code
    /// `fetchAvailableModels` endpoint.
    async fn list_models_antigravity(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        let primary = std::env::var("ANTIGRAVITY_API_BASE")
            .unwrap_or_else(|_| "https://daily-cloudcode-pa.googleapis.com".to_string());
        let endpoints = [primary, "https://cloudcode-pa.googleapis.com".to_string()];
        let mut last_err = String::new();
        for (i, base) in endpoints.iter().enumerate() {
            let url = format!("{}/v1internal:fetchAvailableModels", base.trim_end_matches('/'));
            // Bound each attempt (see kiro note above).
            match tokio::time::timeout(
                std::time::Duration::from_secs(20),
                antigravity_fetch_models(&self.client, &url, key, &self.extra_headers),
            )
            .await
            {
                Ok(Ok(models)) if !models.is_empty() => return Ok(models),
                Ok(Ok(_)) => {
                    // Per spec, an empty parse falls through to the fallback
                    // catalog; we still try the secondary endpoint for
                    // robustness before giving up.
                    last_err = format!("endpoint[{i}] {url} returned no parseable models");
                    continue;
                }
                Ok(Err(e)) => {
                    last_err = e.to_string();
                    warn!("antigravity fetchAvailableModels attempt {i} failed: {e}");
                    continue;
                }
                Err(_) => {
                    last_err = format!("endpoint[{i}] timed out after 20s");
                    warn!("antigravity fetchAvailableModels attempt {i} timed out");
                    continue;
                }
            }
        }
        warn!(
            "antigravity list_models failed ({}); serving static fallback catalog",
            last_err
        );
        Ok(ANTIGRAVITY_FALLBACK
            .iter()
            .map(|id| RawModel { id: id.to_string(), name: None })
            .collect())
    }
}

#[async_trait]
impl Provider for DeviceProvider {
    async fn list_models(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        match self.kind.as_str() {
            "kiro" => self.list_models_kiro(key).await,
            "antigravity" => self.list_models_antigravity(key).await,
            _ => {
                // Generic device provider: original openai-compat style
                // `GET {base}/models`.
                let url = format!("{}/models", self.base_url.trim_end_matches('/'));
                let mut req = self.client
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", key.expose()));
                for (k, v) in self.extra_headers.iter() {
                    req = req.header(k, v);
                }
                let resp = req.send().await?;
                let status = resp.status();
                if !status.is_success() {
                    let txt = resp
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
                    anyhow::bail!("list_models {}: {}", status, txt);
                }
                let v: serde_json::Value = resp.json().await?;
                let mut out = Vec::new();
                if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                    for item in arr {
                        if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                            out.push(RawModel { id: id.to_string(), name: item.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()) });
                        }
                    }
                } else if let Some(arr) = v.get("models").and_then(|d| d.as_array()) {
                    for item in arr {
                        if let Some(id) = item.get("id").and_then(|x| x.as_str()).or_else(|| item.get("name").and_then(|x| x.as_str())) {
                            out.push(RawModel { id: id.to_string(), name: None });
                        }
                    }
                } else if let Some(arr) = v.as_array() {
                    for item in arr {
                        if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                            out.push(RawModel { id: id.to_string(), name: None });
                        } else if let Some(s) = item.as_str() {
                            out.push(RawModel { id: s.to_string(), name: None });
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    async fn send(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut req = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", ctx.api_key.expose()))
            .header("Content-Type", "application/json")
            .json(&ctx.body);
        for (k, v) in self.extra_headers.iter() {
            req = req.header(k, v);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        Ok(UpstreamResponse { status, headers, is_stream: ctx.stream, response: resp })
    }

    async fn send_images(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse> {
        let url = format!("{}/images/generations", self.base_url.trim_end_matches('/'));
        let mut req = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", ctx.api_key.expose()))
            .header("Content-Type", "application/json")
            .json(&ctx.body);
        for (k, v) in self.extra_headers.iter() {
            req = req.header(k, v);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        Ok(UpstreamResponse { status, headers, is_stream: false, response: resp })
    }

    fn protocol(&self) -> xrouter_core::Protocol { xrouter_core::Protocol::OpenAiCompat }
    fn base_url(&self) -> &str { &self.base_url }
}

/// True if `kind` is a device-login provider (`kiro`, `antigravity`,
/// `device:<name>`).
pub fn is_device_kind(kind: &str) -> bool {
    xrouter_auth::is_device_kind(kind)
}

/// Bare provider name for a device `kind` (`device:kiro` -> `kiro`).
pub fn device_provider_name(kind: &str) -> &str {
    xrouter_auth::normalize_provider(kind)
}

/// Extra static headers required on upstream requests for a device provider.
pub fn device_extra_headers(kind: &str) -> reqwest::header::HeaderMap {
    xrouter_auth::upstream_extra_headers(kind)
}

/// Factory
pub fn make_provider(kind: &str, base_url: String, client: Client) -> Box<dyn Provider> {
    match kind {
        "anthropic" => Box::new(AnthropicAdapter { base_url, client }),
        k if is_device_kind(k) => {
            Box::new(DeviceProvider::new(
                base_url,
                client,
                device_extra_headers(k),
                device_provider_name(k).to_string(),
                None,
            ))
        }
        _ => Box::new(OpenAiCompatAdapter { base_url, client }),
    }
}

/// Static fallback catalog served when kiro model discovery fails entirely.
const KIRO_FALLBACK: &[&str] = &[
    "claude-sonnet-4.5",
    "claude-haiku-4.5",
    "claude-sonnet-4",
    "claude-opus-4.1",
    "claude-opus-4",
    "claude-3-7-sonnet",
];

/// Static fallback catalog served when antigravity model discovery fails
/// entirely or yields no parseable models.
const ANTIGRAVITY_FALLBACK: &[&str] = &[
    "gemini-3-pro-preview",
    "gemini-3-flash-preview",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "claude-sonnet-4-5-thinking",
    "claude-opus-4-6-thinking",
];

/// Derive the kiro/CodeWhisperer AWS region from a profile ARN of the form
/// `arn:aws:codewhisperer:{region}:...`. Falls back to `us-east-1` when the
/// ARN is absent or malformed.
pub fn kiro_region_from_arn(profile_arn: Option<&str>) -> String {
    const DEFAULT: &str = "us-east-1";
    let arn = match profile_arn {
        Some(a) if !a.is_empty() => a,
        _ => return DEFAULT.to_string(),
    };
    let parts: Vec<&str> = arn.split(':').collect();
    if parts.len() >= 4
        && parts[1] == "aws"
        && parts[2] == "codewhisperer"
        && !parts[3].is_empty()
    {
        parts[3].to_string()
    } else {
        DEFAULT.to_string()
    }
}

/// Base URL for the kiro `ListAvailableModels` endpoint. Env-overridable via
/// `KIRO_Q_BASE_URL`; otherwise derived from the runtime region.
pub fn kiro_q_base_url(region: &str) -> String {
    std::env::var("KIRO_Q_BASE_URL")
        .unwrap_or_else(|_| format!("https://q.{}.amazonaws.com", region))
}

/// Fetch and parse kiro `ListAvailableModels`.
async fn kiro_fetch_models(
    client: &Client,
    url: &str,
    key: &ApiKey,
) -> anyhow::Result<Vec<RawModel>> {
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", key.expose()))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
        anyhow::bail!("kiro list_models {}: {}", status, txt);
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(parse_kiro_models(&v))
}

/// Parse a kiro `ListAvailableModels` response. The documented shape is
/// `{ "models": [ { "modelId": "...", "modelName"?: ..., "tokenLimits"?: {...} } ] }`.
pub fn parse_kiro_models(v: &serde_json::Value) -> Vec<RawModel> {
    let mut out = Vec::new();
    if let Some(arr) = v.get("models").and_then(|m| m.as_array()) {
        for item in arr {
            if let Some(id) = item.get("modelId").and_then(|x| x.as_str()) {
                let name = item
                    .get("modelName")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
                out.push(RawModel { id: id.to_string(), name });
            }
        }
    }
    out
}

/// Fetch and defensively parse antigravity `fetchAvailableModels`.
async fn antigravity_fetch_models(
    client: &Client,
    url: &str,
    key: &ApiKey,
    extra_headers: &reqwest::header::HeaderMap,
) -> anyhow::Result<Vec<RawModel>> {
    let mut req = client
        .post(url)
        .header("Authorization", format!("Bearer {}", key.expose()))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}));
    for (k, v) in extra_headers.iter() {
        req = req.header(k, v);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
        anyhow::bail!("antigravity list_models {}: {}", status, txt);
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(parse_antigravity_models(&v))
}

/// Defensively parse an antigravity `fetchAvailableModels` response. The shape
/// is not stable across versions, so we look for model arrays under `models`
/// or `availableModels` (and accept a top-level array), and read each entry's
/// id from `modelId`, `id`, or `name`.
pub fn parse_antigravity_models(v: &serde_json::Value) -> Vec<RawModel> {
    let mut out = Vec::new();
    let mut items: Vec<&serde_json::Value> = Vec::new();
    for key in ["models", "availableModels"] {
        if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
            items.extend(arr.iter());
        }
    }
    if let Some(arr) = v.as_array() {
        items.extend(arr.iter());
    }
    for item in items {
        let id = item
            .get("modelId")
            .and_then(|x| x.as_str())
            .or_else(|| item.get("id").and_then(|x| x.as_str()))
            .or_else(|| item.get("name").and_then(|x| x.as_str()));
        if let Some(id) = id {
            let name = item
                .get("name")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            out.push(RawModel { id: id.to_string(), name });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiro_region_from_arn_parses_region() {
        assert_eq!(
            kiro_region_from_arn(Some(
                "arn:aws:codewhisperer:eu-central-1:123456789012:profile/abc"
            )),
            "eu-central-1"
        );
        assert_eq!(kiro_region_from_arn(None), "us-east-1");
        assert_eq!(kiro_region_from_arn(Some("")), "us-east-1");
        assert_eq!(
            kiro_region_from_arn(Some("arn:aws:something:us-west-2:account")),
            "us-east-1"
        );
    }

    #[test]
    fn kiro_parse_models_array() {
        let v = serde_json::json!({
            "models": [
                { "modelId": "claude-sonnet-4.5", "modelName": "Claude Sonnet 4.5" },
                { "modelId": "claude-haiku-4.5" }
            ]
        });
        let models = parse_kiro_models(&v);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-sonnet-4.5");
        assert_eq!(models[0].name.as_deref(), Some("Claude Sonnet 4.5"));
        assert_eq!(models[1].id, "claude-haiku-4.5");
        assert_eq!(models[1].name, None);
    }

    #[test]
    fn kiro_parse_models_empty() {
        assert!(parse_kiro_models(&serde_json::json!({ "models": [] })).is_empty());
        assert!(parse_kiro_models(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn antigravity_defensive_parse_models() {
        // shape 1: models array with modelId
        let v1 = serde_json::json!({
            "models": [
                { "modelId": "gemini-3-pro-preview" },
                { "modelId": "gemini-3-flash-preview" }
            ]
        });
        let m1 = parse_antigravity_models(&v1);
        assert_eq!(m1.len(), 2);
        assert_eq!(m1[0].id, "gemini-3-pro-preview");

        // shape 2: availableModels with id
        let v2 = serde_json::json!({
            "availableModels": [
                { "id": "gemini-2.5-pro" },
                { "id": "gemini-2.5-flash" }
            ]
        });
        let m2 = parse_antigravity_models(&v2);
        assert_eq!(m2.len(), 2);
        assert_eq!(m2[0].id, "gemini-2.5-pro");

        // shape 3: top-level array, id under name then modelId
        let v3 = serde_json::json!([
            { "name": "claude-sonnet-4-5-thinking" },
            { "modelId": "claude-opus-4-6-thinking" }
        ]);
        let m3 = parse_antigravity_models(&v3);
        assert_eq!(m3.len(), 2);
        assert_eq!(m3[0].id, "claude-sonnet-4-5-thinking");
        assert_eq!(m3[1].id, "claude-opus-4-6-thinking");

        // shape 4: unrecognized -> empty
        assert!(parse_antigravity_models(&serde_json::json!({})).is_empty());
        assert!(parse_antigravity_models(&serde_json::json!({ "foo": "bar" })).is_empty());
    }

    #[test]
    fn fallback_catalogs_match_spec() {
        assert_eq!(KIRO_FALLBACK.len(), 6);
        assert_eq!(ANTIGRAVITY_FALLBACK.len(), 6);
    }
}
