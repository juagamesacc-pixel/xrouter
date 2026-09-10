pub mod cache;
pub use cache::ModelCache;

use std::sync::Mutex;
use std::time::{Duration, Instant};
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

// ── OpenCode CLI Session Emulator ──────────────────────────────────────────

struct OpenCodeSessionState {
    session_id: String,
    last_seen: Instant,
}

static OPENCODE_SESSION: Mutex<Option<OpenCodeSessionState>> = Mutex::new(None);

/// Generates headers identical to the official OpenCode CLI:
/// 1. Stable `x-opencode-session` across the conversation (sticky caching & no RPM penalty).
/// 2. Rotates to a new session if idle for >30 minutes.
/// 3. Unique `x-opencode-request` per HTTP call.
fn get_opencode_cli_headers() -> (String, String, String) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static REQ_COUNTER: AtomicU64 = AtomicU64::new(1);

    let mut lock = OPENCODE_SESSION.lock().unwrap();
    let now = Instant::now();

    let session_id = match &mut *lock {
        Some(s) if now.duration_since(s.last_seen) < Duration::from_secs(1800) => {
            s.last_seen = now;
            s.session_id.clone()
        }
        _ => {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let new_id = format!("ses_{:x}_{:x}", std::process::id(), t);
            *lock = Some(OpenCodeSessionState {
                session_id: new_id.clone(),
                last_seen: now,
            });
            new_id
        }
    };

    let req_num = REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let req_id = format!(
        "msg_{:x}_{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        req_num
    );

    (session_id, "global".to_string(), req_id)
}

// ── OpenAI-Compatible Adapter ──────────────────────────────────────────────

pub struct OpenAiCompatAdapter {
    pub base_url: String,
    pub client: Client,
}

impl OpenAiCompatAdapter {
    pub fn new(base_url: String, client: Client) -> Self {
        Self { base_url, client }
    }
}

#[async_trait]
impl Provider for OpenAiCompatAdapter {
    async fn list_models(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let mut req = self.client.get(&url)
            .header("Authorization", format!("Bearer {}", key.expose()));

        if self.base_url.contains("opencode.ai") {
            let (session_id, project_id, request_id) = get_opencode_cli_headers();
            req = req
                .header("User-Agent", "opencode/latest/1.3.15/cli")
                .header("x-opencode-client", "cli")
                .header("x-opencode-session", &session_id)
                .header("x-opencode-project", &project_id)
                .header("x-opencode-request", &request_id)
                .header("X-Session-ID", &session_id);
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
                    out.push(RawModel {
                        id: id.to_string(),
                        name: item.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()),
                    });
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
        let mut req = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", ctx.api_key.expose()))
            .header("Content-Type", "application/json")
            .json(&ctx.body);

        if ctx.stream {
            req = req.header("Accept", "text/event-stream");
        }

        if self.base_url.contains("opencode.ai") || ctx.provider == "opencode-zen" {
            let (session_id, project_id, request_id) = get_opencode_cli_headers();
            req = req
                .header("User-Agent", "opencode/latest/1.3.15/cli")
                .header("x-opencode-client", "cli")
                .header("x-opencode-session", &session_id)
                .header("x-opencode-project", &project_id)
                .header("x-opencode-request", &request_id)
                .header("X-Session-ID", &session_id);
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let is_stream = ctx.stream;
        Ok(UpstreamResponse { status, headers, is_stream, response: resp })
    }

    async fn send_images(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse> {
        let url = format!("{}/images/generations", self.base_url.trim_end_matches('/'));
        let mut req = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", ctx.api_key.expose()))
            .header("Content-Type", "application/json")
            .json(&ctx.body);

        if self.base_url.contains("opencode.ai") || ctx.provider == "opencode-zen" {
            let (session_id, project_id, request_id) = get_opencode_cli_headers();
            req = req
                .header("User-Agent", "opencode/latest/1.3.15/cli")
                .header("x-opencode-client", "cli")
                .header("x-opencode-session", &session_id)
                .header("x-opencode-project", &project_id)
                .header("x-opencode-request", &request_id)
                .header("X-Session-ID", &session_id);
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        Ok(UpstreamResponse { status, headers, is_stream: false, response: resp })
    }

    fn protocol(&self) -> xrouter_core::Protocol {
        xrouter_core::Protocol::OpenAiCompat
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

// ── Anthropic Adapter ──────────────────────────────────────────────────────

pub struct AnthropicAdapter {
    pub base_url: String,
    pub client: Client,
}

impl AnthropicAdapter {
    pub fn new(base_url: String, client: Client) -> Self {
        Self { base_url, client }
    }
}

#[async_trait]
impl Provider for AnthropicAdapter {
    async fn list_models(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        if self.base_url.contains("opencode.ai") {
            let url = format!("{}/models", self.base_url.trim_end_matches('/'));
            let (session_id, project_id, request_id) = get_opencode_cli_headers();
            let resp = self.client.get(&url)
                .header("Authorization", format!("Bearer {}", key.expose()))
                .header("User-Agent", "opencode/latest/1.3.15/cli")
                .header("x-opencode-client", "cli")
                .header("x-opencode-session", &session_id)
                .header("x-opencode-project", &project_id)
                .header("x-opencode-request", &request_id)
                .header("X-Session-ID", &session_id)
                .send().await?;
            let status = resp.status();
            if status.is_success() {
                let v: serde_json::Value = resp.json().await?;
                let mut out = Vec::new();
                if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                    for item in arr {
                        if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                            out.push(RawModel {
                                id: id.to_string(),
                                name: item.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()),
                            });
                        }
                    }
                }
                return Ok(out);
            }
        }
        Ok(vec![])
    }

    async fn send(&self, ctx: &RequestCtx) -> anyhow::Result<UpstreamResponse> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut req = self.client.post(&url)
            .header("x-api-key", ctx.api_key.expose())
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&ctx.body);

        if ctx.stream {
            req = req.header("Accept", "text/event-stream");
        }

        if self.base_url.contains("opencode.ai") || ctx.provider == "opencode-zen" {
            let (session_id, project_id, request_id) = get_opencode_cli_headers();
            req = req
                .header("User-Agent", "opencode/latest/1.3.15/cli")
                .header("x-opencode-client", "cli")
                .header("x-opencode-session", &session_id)
                .header("x-opencode-project", &project_id)
                .header("x-opencode-request", &request_id)
                .header("X-Session-ID", &session_id);
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        Ok(UpstreamResponse { status, headers, is_stream: ctx.stream, response: resp })
    }

    fn protocol(&self) -> xrouter_core::Protocol {
        xrouter_core::Protocol::Anthropic
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

// ── Device Provider (Kiro / Antigravity) ────────────────────────────────────

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

    async fn list_models_kiro(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        let primary_region = kiro_region_from_arn(self.profile_arn.as_deref());
        let regions: Vec<String> = if primary_region == "us-east-1" {
            vec!["us-east-1".to_string()]
        } else {
            vec![primary_region, "us-east-1".to_string()]
        };
        let mut last_err = String::new();
        for region in &regions {
            let base = kiro_q_base_url(region);
            let url = format!("{}/ListAvailableModels?origin=AI_EDITOR", base.trim_end_matches('/'));
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

    async fn list_models_antigravity(&self, key: &ApiKey) -> anyhow::Result<Vec<RawModel>> {
        let primary = std::env::var("ANTIGRAVITY_API_BASE")
            .unwrap_or_else(|_| "https://daily-cloudcode-pa.googleapis.com".to_string());
        let endpoints = [primary, "https://cloudcode-pa.googleapis.com".to_string()];
        let mut last_err = String::new();
        for (i, base) in endpoints.iter().enumerate() {
            let url = format!("{}/v1internal:fetchAvailableModels", base.trim_end_matches('/'));
            match tokio::time::timeout(
                std::time::Duration::from_secs(20),
                antigravity_fetch_models(&self.client, &url, key, &self.extra_headers),
            )
            .await
            {
                Ok(Ok(models)) if !models.is_empty() => return Ok(models),
                Ok(Ok(_)) => {
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
        let normalized = device_provider_name(&self.kind).to_string();
        match normalized.as_str() {
            "kiro" => self.list_models_kiro(key).await,
            "antigravity" => self.list_models_antigravity(key).await,
            _ => {
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

pub fn is_device_kind(kind: &str) -> bool {
    xrouter_auth::is_device_kind(kind)
}

pub fn device_provider_name(kind: &str) -> &str {
    xrouter_auth::normalize_provider(kind)
}

pub fn device_extra_headers(kind: &str) -> reqwest::header::HeaderMap {
    xrouter_auth::upstream_extra_headers(kind)
}

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

const KIRO_FALLBACK: &[&str] = &[
    "claude-sonnet-4.5",
    "claude-haiku-4.5",
    "claude-sonnet-4",
    "claude-opus-4.1",
    "claude-opus-4",
    "claude-3-7-sonnet",
];

const ANTIGRAVITY_FALLBACK: &[&str] = &[
    "gemini-3-pro-preview",
    "gemini-3-flash-preview",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "claude-sonnet-4-5-thinking",
    "claude-opus-4-6-thinking",
];

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

pub fn kiro_q_base_url(region: &str) -> String {
    std::env::var("KIRO_Q_BASE_URL")
        .unwrap_or_else(|_| format!("https://q.{}.amazonaws.com", region))
}

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
        let v1 = serde_json::json!({
            "models": [
                { "modelId": "gemini-3-pro-preview" },
                { "modelId": "gemini-3-flash-preview" }
            ]
        });
        let m1 = parse_antigravity_models(&v1);
        assert_eq!(m1.len(), 2);
        assert_eq!(m1[0].id, "gemini-3-pro-preview");

        let v2 = serde_json::json!({
            "availableModels": [
                { "id": "gemini-2.5-pro" },
                { "id": "gemini-2.5-flash" }
            ]
        });
        let m2 = parse_antigravity_models(&v2);
        assert_eq!(m2.len(), 2);
        assert_eq!(m2[0].id, "gemini-2.5-pro");

        let v3 = serde_json::json!([
            { "name": "claude-sonnet-4-5-thinking" },
            { "modelId": "claude-opus-4-6-thinking" }
        ]);
        let m3 = parse_antigravity_models(&v3);
        assert_eq!(m3.len(), 2);
        assert_eq!(m3[0].id, "claude-sonnet-4-5-thinking");
        assert_eq!(m3[1].id, "claude-opus-4-6-thinking");

        assert!(parse_antigravity_models(&serde_json::json!({})).is_empty());
        assert!(parse_antigravity_models(&serde_json::json!({ "foo": "bar" })).is_empty());
    }

    #[test]
    fn fallback_catalogs_match_spec() {
        assert_eq!(KIRO_FALLBACK.len(), 6);
        assert_eq!(ANTIGRAVITY_FALLBACK.len(), 6);
    }

    #[test]
    fn device_kind_normalization_routes_correctly() {
        assert_eq!(device_provider_name("kiro"), "kiro");
        assert_eq!(device_provider_name("device:kiro"), "kiro");
        assert_eq!(device_provider_name("antigravity"), "antigravity");
        assert_eq!(device_provider_name("device:antigravity"), "antigravity");
        assert_eq!(device_provider_name("device:somethingelse"), "somethingelse");
    }
}
