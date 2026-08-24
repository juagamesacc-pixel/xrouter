pub mod translate;
pub mod metrics;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::{
    body::Body,
    extract::{State, Query},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response, Json},
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use serde_json::{Value, json};
use tracing::{info, warn};

use metrics::{Metrics, SharedMetrics};
use xrouter_config::{Config, load};
use xrouter_balancer::{Balancer, EndpointId};
use xrouter_providers::{make_provider, ModelCache, RawModel, RequestCtx};
use xrouter_core::is_free;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<Config>>,
    pub balancer: Arc<Balancer>,
    pub client: reqwest::Client,
    pub metrics: SharedMetrics,
    pub model_cache: ModelCache,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        let balancer = Arc::new(Balancer::new());
        for (prov, pcfg) in &cfg.providers {
            if pcfg.enabled && !pcfg.keys.is_empty() {
                let keys = pcfg.keys.iter().map(|k| xrouter_core::ApiKey(k.clone())).collect::<Vec<_>>();
                balancer.update_keys(prov, keys);
            }
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(3))
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .build().unwrap();
        Self {
            config: Arc::new(RwLock::new(cfg)),
            balancer,
            client,
            metrics: Arc::new(Metrics::default()),
            model_cache: ModelCache::default(),
        }
    }

    /// Populate the model cache by querying each configured provider once.
    /// This is the only place that performs a live `list_models` fetch; the
    /// `/admin/models` endpoint serves from this cache instead of fetching live.
    pub async fn refresh_model_cache(&self) {
        let cfg = self.get_config();
        for (prov, pcfg) in &cfg.providers {
            if pcfg.keys.is_empty() { continue; }
            let key = xrouter_core::ApiKey(pcfg.keys[0].clone());
            let adapter = make_provider(&pcfg.kind, pcfg.base_url.clone(), self.client.clone());
            match adapter.list_models(&key).await {
                Ok(models) => {
                    self.model_cache.insert(prov.clone(), models);
                    tracing::info!(provider = prov, "model cache refreshed");
                }
                Err(e) => warn!("model cache refresh failed for {}: {}", prov, e),
            }
        }
    }

    pub fn reload(&self) -> anyhow::Result<()> {
        let cfg = load()?;
        for (prov, pcfg) in &cfg.providers {
            if pcfg.enabled && !pcfg.keys.is_empty() {
                let keys = pcfg.keys.iter().map(|k| xrouter_core::ApiKey(k.clone())).collect::<Vec<_>>();
                self.balancer.update_keys(prov, keys);
            }
        }
        *self.config.write().unwrap() = cfg;
        Ok(())
    }
    pub fn get_config(&self) -> Config {
        self.config.read().unwrap().clone()
    }
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/messages", post(handle_messages))
        .route("/v1/models", get(handle_models))
        .route("/healthz", get(handle_healthz))
        .route("/admin/tiers", get(handle_admin_tiers))
        .route("/admin/models", get(handle_admin_models))
        .route("/admin/reload", post(handle_reload))
        .route("/admin/metrics", get(handle_metrics))
        .route("/admin/stats", get(handle_stats))
        .with_state(state)
}

async fn handle_healthz() -> impl IntoResponse { (StatusCode::OK, Json(json!({"status":"ok"}))) }

async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.get_config();
    let models: Vec<Value> = cfg.tiers.iter().map(|t| json!({"id": t.name, "object":"model","owned_by":"xrouter"} )).collect();
    Json(json!({"object":"list","data": models}))
}

#[derive(serde::Deserialize)]
struct ModelsQuery { provider: Option<String>, free: Option<bool> }

async fn handle_admin_models(State(state): State<AppState>, Query(q): Query<ModelsQuery>) -> Response {
    let cfg = state.get_config();
    let providers_to_query: Vec<String> = if let Some(p) = q.provider { vec![p] } else { cfg.providers.keys().cloned().collect() };
    let mut out: Vec<Value> = Vec::new();
    for prov in providers_to_query {
        if let Some(pcfg) = cfg.providers.get(&prov) {
            if pcfg.keys.is_empty() { continue; }
            // Branch on provider kind. Anthropic exposes no list-models
            // endpoint, so it is served purely from the cache (which stays
            // empty for it). OpenAI-compatible providers are served from the
            // ModelCache rather than performing a live fetch on every request.
            let models: Vec<RawModel> = match pcfg.kind.as_str() {
                "anthropic" => state.model_cache.get(&prov).unwrap_or_default(),
                _ => state.model_cache.get(&prov).unwrap_or_default(),
            };
            for m in models {
                let free = is_free(&m.id);
                if q.free.unwrap_or(false) && !free { continue; }
                out.push(json!({"provider": prov, "id": m.id, "free": free}));
            }
        }
    }
    (StatusCode::OK, Json(json!({"data": out}))).into_response()
}

async fn handle_admin_tiers(State(state): State<AppState>) -> Response {
    let cfg = state.get_config();
    let balancer = &state.balancer;
    let mut tiers_out = Vec::new();
    for tier in &cfg.tiers {
        let mut entries = Vec::new();
        for e in &tier.entries {
            let id = EndpointId::new(&e.provider, &e.model);
            let healthy = balancer.health.is_healthy(&id);
            let state_str = if healthy { "healthy" } else { "cooling" };
            entries.push(json!({"provider": e.provider, "model": e.model, "healthy": healthy, "state": state_str, "is_default": e.is_default, "weight": e.weight}));
        }
        tiers_out.push(json!({"name": tier.name, "strict": tier.strict, "entries": entries}));
    }
    (StatusCode::OK, Json(json!({"tiers": tiers_out}))).into_response()
}

async fn handle_metrics(State(state): State<AppState>) -> Response {
    let body = state.metrics.to_prometheus();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/plain; version=0.0.4"));
    (StatusCode::OK, headers, body).into_response()
}

async fn handle_stats(State(state): State<AppState>) -> Response {
    let cfg = state.get_config();
    let balancer_snapshot: Vec<Value> = state.balancer.health.snapshot().iter().map(|(id, h)| {
        let state_str = match h.state {
            xrouter_balancer::HealthState::Healthy => "healthy",
            xrouter_balancer::HealthState::Cooling{..} => "cooling",
            xrouter_balancer::HealthState::HalfOpen => "half_open",
        };
        json!({"endpoint": id.0, "state": state_str, "failures": h.consecutive_failures})
    }).collect();
    let resp = json!({
        "metrics": state.metrics.to_json(),
        "tiers": cfg.tiers.len(),
        "providers": cfg.providers.len(),
        "health": balancer_snapshot,
    });
    (StatusCode::OK, Json(resp)).into_response()
}

async fn handle_reload(State(state): State<AppState>) -> Response {
    let res = catch_unwind(AssertUnwindSafe(|| state.reload()));
    match res {
        Ok(Ok(_)) => {
            // refresh model cache after config changed
            state.refresh_model_cache().await;
            (StatusCode::OK, Json(json!({"status":"reloaded"}))).into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"panic during reload"}))).into_response(),
    }
}

// --- Core routing helper ---

fn extract_tier_name(body: &Value) -> Option<String> {
    body.get("model").and_then(|m| m.as_str()).map(|s| s.to_string())
}

/// Fast two-phase peek: extract model without full parse, then decide if translation needed.
fn peek_model_fast(bytes: &[u8]) -> Option<String> {
    crate::translate::peek_model(bytes)
}

async fn route_openai_request(state: &AppState, body: Value, raw_bytes: Bytes, headers: HeaderMap, is_anthropic_ingress: bool) -> Response {
    state.metrics.inc_requests();
    let start = std::time::Instant::now();
    // auth check
    {
        let cfg = state.get_config();
        if let Some(token) = cfg.settings.api_token.clone() {
            if !token.is_empty() {
                let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
                if auth != format!("Bearer {}", token) {
                    state.metrics.inc_error();
                    return (StatusCode::UNAUTHORIZED, Json(json!({"error":{"type":"authentication_error","message":"invalid router token"}}))).into_response();
                }
            }
        }
    }

    // Two-phase parse: peek model first for metrics/tracing without full Value clone when protocols match
    let tier_name = match extract_tier_name(&body) {
        Some(n) => n,
        None => {
            // fallback try peek fast
            if let Some(m) = peek_model_fast(&raw_bytes) { m } else {
                let err = if is_anthropic_ingress {
                    json!({"type":"error","error":{"type":"invalid_request_error","message":"missing model field"}})
                } else {
                    json!({"error":{"type":"invalid_request_error","message":"missing model field"}})
                };
                state.metrics.inc_error();
                return (StatusCode::BAD_REQUEST, Json(err)).into_response();
            }
        }
    };

    let cfg = state.get_config();
    let tier = match cfg.tiers.iter().find(|t| t.name == tier_name) {
        Some(t) => t.clone(),
        None => {
            let available = cfg.tiers.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
            let body_json = if is_anthropic_ingress {
                json!({"type":"error","error":{"type":"not_found_error","message": format!("unknown tier '{}'", tier_name)}})
            } else {
                xrouter_core::unknown_tier_body(&tier_name, &available)
            };
            state.metrics.inc_error();
            return (StatusCode::NOT_FOUND, Json(body_json)).into_response();
        }
    };

    let streaming = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let candidates = state.balancer.candidates_ordered(&tier);
    if candidates.is_empty() {
        state.metrics.inc_tier_exhausted();
        let body_json = if is_anthropic_ingress {
            json!({"type":"error","error":{"type":"overloaded_error","message": format!("all models/providers for tier '{}' failed", tier_name)}})
        } else {
            xrouter_core::tier_exhausted_body(&tier_name)
        };
        return (StatusCode::SERVICE_UNAVAILABLE, Json(body_json)).into_response();
    }
    // Allow retrying each candidate entry once per available key, but cap the
    // total number of attempts so a large fan-out cannot loop forever.
    let keys_per_provider = candidates.iter().map(|e| state.balancer.keys_len(&e.provider)).max().unwrap_or(1).max(1);
    let max_attempts = std::cmp::min(candidates.len() * keys_per_provider, 6).max(1);
    let mut last_error: Option<(u16, String)> = None;
    let mut attempt = 0usize;

    for entry in candidates.iter().take(max_attempts) {
        let key_opt = state.balancer.next_key(&entry.provider);
        let (key, _idx) = match key_opt {
            Some(k) => k,
            None => {
                last_error = Some((401, "no valid keys".to_string()));
                state.metrics.inc_error();
                continue;
            }
        };
        let pcfg = match cfg.providers.get(&entry.provider) {
            Some(p) => p.clone(),
            None => continue,
        };
        if !pcfg.enabled { continue; }

        let upstream_is_anthropic = pcfg.kind == "anthropic";
        // Fast path: when protocols match, we can avoid re-serialization? Here we still need to set model correctly.
        // Use raw_bytes when possible: if protocols match and no other translation needed, we could forward raw_bytes with model replaced.
        // For simplicity, translate via Value but keep allocation minimal.
        let translated_body = if is_anthropic_ingress && !upstream_is_anthropic {
            crate::translate::anthropic_to_openai(&body, &entry.model)
        } else if !is_anthropic_ingress && upstream_is_anthropic {
            crate::translate::openai_to_anthropic(&body, &entry.model)
        } else if !is_anthropic_ingress && !upstream_is_anthropic {
            let mut v = body.clone();
            v["model"] = Value::String(entry.model.clone());
            v
        } else {
            let mut v = body.clone();
            v["model"] = Value::String(entry.model.clone());
            v
        };

        let endpoint_id = EndpointId::new(&entry.provider, &entry.model);
        let ctx = RequestCtx {
            provider: entry.provider.clone(),
            base_url: pcfg.base_url.clone(),
            model: entry.model.clone(),
            api_key: key.clone(),
            body: translated_body.clone(),
            stream: streaming,
        };

        // panic isolation around adapter creation/send
        let adapter: Box<dyn xrouter_providers::Provider> = xrouter_providers::make_provider(&pcfg.kind, pcfg.base_url.clone(), state.client.clone());

        // per-attempt timeout: connect 3s via client, TTFB 15s, total handled via timeout
        let fut = adapter.send(&ctx);
        // Use select with client disconnect? For now just timeout 15s TTFB
        let res = tokio::time::timeout(Duration::from_secs(15), fut).await;
        let upstream = match res {
            Ok(Ok(u)) => u,
            Ok(Err(e)) => {
                warn!("upstream error {}: {}", entry.provider, e);
                state.balancer.health.mark_failure(&endpoint_id);
                state.metrics.inc_retries();
                last_error = Some((502, e.to_string()));
                attempt += 1;
                if attempt >= max_attempts { break; }
                continue;
            }
            Err(_) => {
                warn!("timeout {}", entry.provider);
                state.balancer.health.mark_failure(&endpoint_id);
                state.metrics.inc_timeouts();
                state.metrics.inc_retries();
                last_error = Some((504, "upstream timeout".to_string()));
                attempt += 1;
                if attempt >= max_attempts { break; }
                continue;
            }
        };

        // Check status with retry classification
        if upstream.status >= 200 && upstream.status < 300 {
            state.balancer.health.mark_success(&endpoint_id);
            state.metrics.inc_success();
            tracing::info!(tier=tier_name, provider=entry.provider, model=entry.model, latency_ms=start.elapsed().as_millis() as u64, status=upstream.status, "request success");
            if streaming {
                let need_translate = is_anthropic_ingress != upstream_is_anthropic;
                if need_translate {
                    // Translation requires the full payload, so buffer once.
                    let body_bytes = upstream.response.bytes().await.unwrap_or_default();
                    let body_text = String::from_utf8_lossy(&body_bytes).to_string();
                    let translated = translate_stream_body(&body_text, !upstream_is_anthropic, is_anthropic_ingress);
                    let headers_out = build_sse_headers();
                    return (StatusCode::OK, headers_out, Body::from(translated)).into_response();
                } else {
                    let headers_out = build_sse_headers();
                    // Real streaming passthrough: forward upstream bytes stream
                    // without buffering the whole body first.
                    let stream = upstream.response.bytes_stream();
                    return (StatusCode::OK, headers_out, Body::from_stream(stream)).into_response();
                }
            } else {
                let body_bytes = upstream.response.bytes().await.unwrap_or_default();
                let translated = if is_anthropic_ingress != upstream_is_anthropic {
                    translate_non_stream_response(&body_bytes, is_anthropic_ingress, upstream_is_anthropic)
                } else {
                    body_bytes
                };
                let mut headers_out = HeaderMap::new();
                headers_out.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
                return (StatusCode::OK, headers_out, Body::from(translated)).into_response();
            }
        } else if upstream.status == 401 || upstream.status == 403 {
            state.balancer.health.mark_key_dead(key.expose());
            let body_bytes = upstream.response.bytes().await.unwrap_or_default();
            last_error = Some((upstream.status, String::from_utf8_lossy(&body_bytes).to_string()));
            state.metrics.inc_retries();
            attempt += 1;
            continue;
        } else if upstream.status == 429 {
            // honor Retry-After
            let retry_after = upstream.headers.get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            if let Some(ref ra) = retry_after {
                state.balancer.health.apply_retry_after(&endpoint_id, Some(ra));
            } else {
                state.balancer.health.mark_failure(&endpoint_id);
            }
            let body_bytes = upstream.response.bytes().await.unwrap_or_default();
            last_error = Some((upstream.status, String::from_utf8_lossy(&body_bytes).to_string()));
            state.metrics.inc_retries();
            attempt += 1;
            if let Some(ra) = retry_after {
                if let Ok(secs) = ra.parse::<u64>() {
                    if secs > 0 && secs < 60 {
                        tokio::time::sleep(Duration::from_secs(secs)).await;
                    }
                }
            }
            continue;
        } else if upstream.status >= 500 || upstream.status == 408 {
            state.balancer.health.mark_failure(&endpoint_id);
            let body_bytes = upstream.response.bytes().await.unwrap_or_default();
            last_error = Some((upstream.status, String::from_utf8_lossy(&body_bytes).to_string()));
            state.metrics.inc_retries();
            attempt += 1;
            if attempt >= max_attempts { break; }
            // jitter 0-25ms
            let jitter = (attempt as u64 * 5) % 25;
            tokio::time::sleep(Duration::from_millis(jitter)).await;
            continue;
        } else {
            let body_bytes = upstream.response.bytes().await.unwrap_or_default();
            last_error = Some((upstream.status, String::from_utf8_lossy(&body_bytes).to_string()));
            state.metrics.inc_retries();
            attempt += 1;
            continue;
        }
    }

    // exhausted — strict tier semantics: never degrade to another tier
    state.metrics.inc_tier_exhausted();
    state.metrics.inc_error();
    let status = if let Some((code, _)) = last_error {
        if code == 401 || code == 403 { StatusCode::UNAUTHORIZED } else if code == 504 { StatusCode::GATEWAY_TIMEOUT } else if code == 429 { StatusCode::TOO_MANY_REQUESTS } else { StatusCode::SERVICE_UNAVAILABLE }
    } else { StatusCode::SERVICE_UNAVAILABLE };

    // map to ingress protocol error schema
    let body_json = if is_anthropic_ingress {
        if status == StatusCode::UNAUTHORIZED {
            json!({"type":"error","error":{"type":"authentication_error","message": "no valid keys"}})
        } else if status == StatusCode::GATEWAY_TIMEOUT {
            json!({"type":"error","error":{"type":"overloaded_error","message": format!("all models/providers for tier '{}' failed", tier_name)}})
        } else {
            json!({"type":"error","error":{"type":"overloaded_error","message": format!("all models/providers for tier '{}' failed", tier_name)}})
        }
    } else {
        if status == StatusCode::UNAUTHORIZED {
            json!({"error":{"type":"no_valid_keys","message":"no valid keys"}})
        } else if status == StatusCode::GATEWAY_TIMEOUT {
            json!({"error":{"type":"upstream_timeout","message":"upstream timeout"}})
        } else {
            xrouter_core::tier_exhausted_body(&tier_name)
        }
    };
    let _ = raw_bytes;
    tracing::warn!(tier=tier_name, latency_ms=start.elapsed().as_millis() as u64, "tier exhausted");
    (status, Json(body_json)).into_response()
}

fn build_sse_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-cache"));
    h.insert(header::CONNECTION, header::HeaderValue::from_static("keep-alive"));
    h.insert(header::HeaderName::from_static("x-accel-buffering"), header::HeaderValue::from_static("no"));
    h
}

fn translate_stream_body(body_text: &str, upstream_is_openai: bool, ingress_is_anthropic: bool) -> Bytes {
    let mut out = String::new();
    let mut pending_event: Option<String> = None;
    for line in body_text.lines() {
        if line.starts_with("data: ") {
            let data = line.trim_start_matches("data: ").trim();
            if data == "[DONE]" {
                if ingress_is_anthropic {
                    out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
                } else {
                    out.push_str("data: [DONE]\n\n");
                }
                pending_event = None;
                continue;
            }
            if upstream_is_openai && ingress_is_anthropic {
                let chunks = crate::translate::translate_sse_openai_to_anthropic_chunk(data);
                for c in chunks { out.push_str(&c); out.push_str("\n\n"); }
            } else if !upstream_is_openai {
                // anthropic upstream, ingress openai
                let ev = pending_event.take().unwrap_or_else(|| "content_block_delta".to_string());
                if let Some(translated) = crate::translate::translate_sse_anthropic_to_openai_chunk(&ev, data) {
                    out.push_str(&translated); out.push_str("\n");
                }
            } else {
                out.push_str(line); out.push('\n'); out.push('\n');
            }
        } else if line.starts_with("event:") {
            let ev = line.trim_start_matches("event:").trim().to_string();
            if !upstream_is_openai && !ingress_is_anthropic {
                pending_event = Some(ev);
                continue;
            } else if ingress_is_anthropic && upstream_is_openai {
                continue;
            } else {
                out.push_str(line); out.push('\n');
            }
        } else if line.is_empty() {
            continue;
        } else if line.starts_with(":") {
            // SSE comment keepalive
            out.push_str(line); out.push('\n');
        }
    }
    Bytes::from(out)
}

fn translate_non_stream_response(body: &[u8], ingress_is_anthropic: bool, upstream_is_anthropic: bool) -> Bytes {
    // catch_unwind around translation so bad payload doesn't panic worker
    let result = catch_unwind(AssertUnwindSafe(|| {
        let v: Value = serde_json::from_slice(body).unwrap_or(json!({}));
        let out = if !ingress_is_anthropic && upstream_is_anthropic {
            let text = v.get("content").and_then(|c| c.as_array()).and_then(|arr| arr.first()).and_then(|b| b.get("text")).and_then(|t| t.as_str()).unwrap_or("");
            let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
            json!({
                "id": v.get("id").and_then(|x| x.as_str()).unwrap_or("chatcmpl-translated"),
                "object": "chat.completion",
                "created": 0,
                "model": model,
                "choices": [{ "index": 0, "message": {"role":"assistant","content": text}, "finish_reason": "stop" }],
                "usage": v.get("usage").cloned().unwrap_or(json!({}))
            })
        } else if ingress_is_anthropic && !upstream_is_anthropic {
            let text = v.get("choices").and_then(|c| c.as_array()).and_then(|arr| arr.first()).and_then(|ch| ch.get("message")).and_then(|m| m.get("content")).and_then(|t| t.as_str())
                .or_else(|| v.get("choices").and_then(|c| c.as_array()).and_then(|arr| arr.first()).and_then(|ch| ch.get("delta")).and_then(|d| d.get("content")).and_then(|t| t.as_str()))
                .unwrap_or("");
            let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
            json!({
                "id": v.get("id").and_then(|x| x.as_str()).unwrap_or("msg_translated"),
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [{ "type":"text","text": text }],
                "stop_reason": "end_turn",
                "usage": v.get("usage").cloned().unwrap_or(json!({}))
            })
        } else {
            return Bytes::copy_from_slice(body);
        };
        Bytes::from(serde_json::to_vec(&out).unwrap())
    }));
    match result {
        Ok(b) => b,
        Err(_) => Bytes::copy_from_slice(body),
    }
}

async fn handle_chat_completions(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // fast peek before full parse for metrics fast path
    let _peek = peek_model_fast(&body);
    let val: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error":{"type":"invalid_request_error","message": e.to_string()}}))).into_response(),
    };
    route_openai_request(&state, val, body, headers, false).await
}

async fn handle_messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let val: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"type":"error","error":{"type":"invalid_request_error","message": e.to_string()}}))).into_response(),
    };
    route_openai_request(&state, val, body, headers, true).await
}

pub async fn run_server(addr: String, state: AppState) -> anyhow::Result<()> {
    // Populate the model cache at startup so /admin/models serves from cache.
    state.refresh_model_cache().await;
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("listening on {}", addr);
    // Graceful shutdown: wait for SIGINT/SIGTERM and drain
    let server = axum::serve(listener, app);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        let mut sigint = signal(SignalKind::interrupt()).unwrap();
        tokio::select! {
            _ = server => {},
            _ = sigterm.recv() => { info!("received SIGTERM, draining"); tokio::time::sleep(Duration::from_millis(500)).await; },
            _ = sigint.recv() => { info!("received SIGINT, draining"); tokio::time::sleep(Duration::from_millis(500)).await; },
        }
    }
    #[cfg(not(unix))]
    {
        server.await?;
    }
    #[cfg(unix)]
    {
        // server already consumed in select, need to handle remaining?
    }
    Ok(())
}

// Fallback non-unix run
pub async fn run_server_simple(addr: String, state: AppState) -> anyhow::Result<()> {
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
