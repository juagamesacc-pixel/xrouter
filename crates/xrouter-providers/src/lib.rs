pub mod cache;
pub use cache::ModelCache;

use serde::{Deserialize, Serialize};
use async_trait::async_trait;
use reqwest::Client;
use xrouter_core::ApiKey;

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
pub struct DeviceProvider {
    pub base_url: String,
    pub client: Client,
    pub extra_headers: reqwest::header::HeaderMap,
}

impl DeviceProvider {
    pub fn new(base_url: String, client: Client, extra_headers: reqwest::header::HeaderMap) -> Self {
        Self { base_url, client, extra_headers }
    }
}

#[async_trait]
impl Provider for DeviceProvider {
    async fn list_models(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
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
            Box::new(DeviceProvider::new(base_url, client, device_extra_headers(k)))
        }
        _ => Box::new(OpenAiCompatAdapter { base_url, client }),
    }
}
