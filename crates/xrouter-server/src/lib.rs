pub mod translate;
pub mod metrics;
pub mod track;

use std::convert::Infallible;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::{State, Query},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response, Json},
    routing::{get, post},
    Router,
};
#[cfg(feature = "bench")]
use axum::extract::Request;
#[cfg(feature = "bench")]
use axum::middleware::{self, Next};
use bytes::Bytes;
use futures::StreamExt;
use futures::Stream;
use std::pin::Pin;
use serde_json::{Value, json};
use tracing::{info, warn};

use arc_swap::ArcSwap;
use metrics::{Metrics, SharedMetrics};
use track::{Tracker, code_map, error_code_for_status, error_string_for_code};
use xrouter_config::{Config, load};
use xrouter_balancer::Balancer;
use xrouter_providers::{make_provider, ModelCache, RawModel, RequestCtx, is_device_kind, device_provider_name};
use xrouter_core::is_free;
use xrouter_auth::DeviceStore;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ArcSwap<Config>>,
    pub balancer: Arc<Balancer>,
    pub client: reqwest::Client,
    pub metrics: SharedMetrics,
    pub model_cache: ModelCache,
    /// Per-request tracker. Disabled (zero overhead) unless `--track` is set.
    pub tracker: Tracker,
    /// Off-RAM device-login token store. Only initialized (loaded from disk)
    /// when at least one configured provider uses the device flow; otherwise
    /// `None` and never touches the token file. Shared via `ArcSwapOption` so
    /// `reload` can swap it atomically for every request clone.
    pub device_store: Arc<arc_swap::ArcSwapOption<tokio::sync::Mutex<DeviceStore>>>,
    #[cfg(feature = "bench")]
    pub bench_enabled: bool,
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
        let device_store = Self::init_device_store(&cfg);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(3))
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .build().unwrap();
        Self {
            config: Arc::new(ArcSwap::new(Arc::new(cfg))),
            balancer,
            client,
            metrics: Arc::new(Metrics::default()),
            model_cache: ModelCache::default(),
            tracker: Tracker::new(false),
            device_store: Arc::new(arc_swap::ArcSwapOption::new(device_store)),
            #[cfg(feature = "bench")]
            bench_enabled: false,
        }
    }

    /// Build the device token store only if a device-flow provider is present.
    /// Returns `None` (zero overhead) otherwise.
    fn init_device_store(cfg: &Config) -> Option<Arc<tokio::sync::Mutex<DeviceStore>>> {
        if !cfg.providers.values().any(|p| is_device_kind(&p.kind)) {
            return None;
        }
        match DeviceStore::load() {
            Ok(s) => Some(Arc::new(tokio::sync::Mutex::new(s))),
            Err(e) => {
                tracing::warn!("failed to load device store: {}", e);
                None
            }
        }
    }

    /// Enable per-request tracking. Only has an effect when called; the
    /// tracker is otherwise a disabled no-op.
    pub fn with_track(mut self, enabled: bool) -> Self {
        self.tracker = Tracker::new(enabled);
        self
    }

    /// Enable/disable the benchmarking middleware + `/admin/bench` endpoint.
    /// Only has an effect when compiled with the `bench` feature.
    #[cfg(feature = "bench")]
    pub fn with_bench(mut self, enabled: bool) -> Self {
        self.bench_enabled = enabled;
        self
    }

    /// Populate the model cache by querying each configured provider once.
    /// This is the only place that performs a live `list_models` fetch; the
    /// `/admin/models` endpoint serves from this cache instead of fetching live.
    pub async fn refresh_model_cache(&self) {
        let cfg = self.get_config();
        for (prov, pcfg) in &cfg.providers {
            if is_device_kind(&pcfg.kind) {
                // Device providers: use a (refreshed) device account token.
                if let Some(store) = self.device_store.load_full() {
                    let mut s = store.lock().await;
                    let name = device_provider_name(&pcfg.kind);
                    if let Some(mut acct) = s.next_account(name) {
                        if let Err(e) = xrouter_auth::refresh_if_needed(&mut acct).await {
                            warn!("device token refresh failed for {}: {}", name, e);
                        }
                        let key = xrouter_core::ApiKey(acct.access_token.clone());
                        let adapter = make_provider(&pcfg.kind, pcfg.base_url.clone(), self.client.clone());
                        match adapter.list_models(&key).await {
                            Ok(models) => {
                                self.model_cache.insert(prov.clone(), models);
                                tracing::info!(provider = prov, "model cache refreshed (device)");
                            }
                            Err(e) => warn!("model cache refresh failed for {}: {}", prov, e),
                        }
                    }
                }
                continue;
            }
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
        // Re-evaluate the device store: load it if a device provider now exists,
        // drop it (off-RAM) if none remain. Shared via ArcSwapOption so every
        // request clone observes the new value.
        let new_store = Self::init_device_store(&cfg);
        self.device_store.store(new_store);
        self.config.store(Arc::new(cfg));
        Ok(())
    }

    /// Lock-free, zero-clone read of the current routing config.
    pub fn get_config(&self) -> Arc<Config> {
        self.config.load_full()
    }
}

#[cfg(feature = "bench")]
async fn handle_bench(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let snapshot = state.metrics.snapshot_bench();
    (StatusCode::OK, Json(snapshot)).into_response()
}

/// Middleware that measures per-request latency, accumulates throughput bytes,
/// emits a per-request `tracing::info!` line, and sets the `X-Response-Time`
/// response header. Only does work when `state.bench_enabled` is true (the
/// middleware is only installed in that case, but the guard keeps it cheap).
#[cfg(feature = "bench")]
async fn bench_latency_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let start = std::time::Instant::now();
    let bytes = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let mut response = next.run(req).await;
    let elapsed = start.elapsed();
    if state.bench_enabled {
        state.metrics.record_latency(elapsed);
        state.metrics.record_bytes(bytes);
        tracing::info!(
            latency_ms = elapsed.as_millis() as u64,
            bytes = bytes,
            "bench request"
        );
        if let Ok(v) = header::HeaderValue::from_str(&elapsed.as_millis().to_string()) {
            response.headers_mut().insert(
                header::HeaderName::from_static("x-response-time"),
                v,
            );
        }
    }
    response
}

pub fn create_router(state: AppState) -> Router {
    #[cfg_attr(not(feature = "bench"), allow(unused_mut))]
    let mut router = Router::new()
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/messages", post(handle_messages))
        .route("/v1/images/generations", post(handle_images_generations))
        .route("/v1/models", get(handle_models))
        .route("/healthz", get(handle_healthz))
        .route("/admin/tiers", get(handle_admin_tiers))
        .route("/admin/models", get(handle_admin_models))
        .route("/admin/reload", post(handle_reload))
        .route("/admin/metrics", get(handle_metrics))
        .route("/admin/stats", get(handle_stats))
        .route("/admin/track", get(handle_admin_track))
        .route("/admin/track/codes", get(handle_admin_track_codes))
        .route("/admin/tiers", post(handle_admin_create_tier));
    #[cfg(feature = "bench")]
    {
        if state.bench_enabled {
            router = router
                .route("/admin/bench", get(handle_bench))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    bench_latency_middleware,
                ));
        }
    }
    router.with_state(state)
}

async fn handle_healthz() -> impl IntoResponse { (StatusCode::OK, Json(json!({"status":"ok"}))) }

async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.get_config();
    let models: Vec<Value> = cfg.tiers.iter().map(|t| json!({"id": t.name, "object":"model","owned_by":"xrouter"} )).collect();
    Json(json!({"object":"list","data": models}))
}

#[derive(serde::Deserialize)]
struct ModelsQuery { provider: Option<String>, free: Option<bool>, #[serde(default, rename = "type")] type_: Option<String> }

/// Heuristic: does a model id look like an image-generation model?
pub fn is_image_model(id: &str) -> bool {
    let n = id.to_lowercase();
    n.contains("flux")
        || n.contains("sdxl")
        || n.contains("dall-e")
        || n.contains("dall_e")
        || n.contains("stable-diffusion")
        || n.contains("stable_diffusion")
        || n.contains("imagen")
        || n.contains("ideogram")
        || n.contains("image")
}

async fn handle_admin_models(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<ModelsQuery>) -> Response {
    if let Some(resp) = check_auth(&state, &headers) { return resp; }
    let cfg = state.get_config();
    let providers_to_query: Vec<String> = if let Some(p) = q.provider { vec![p] } else { cfg.providers.keys().cloned().collect() };
    let want_images = q.type_.as_deref() == Some("images");
    let want_text = q.type_.as_deref() == Some("text");
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
                let is_img = is_image_model(&m.id);
                if want_images && !is_img { continue; }
                if want_text && is_img { continue; }
                out.push(json!({"provider": prov, "id": m.id, "free": free, "image": is_img}));
            }
        }
    }
    (StatusCode::OK, Json(json!({"data": out}))).into_response()
}

async fn handle_admin_tiers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = check_auth(&state, &headers) { return resp; }
    let cfg = state.get_config();
    let balancer = &state.balancer;
    let mut tiers_out = Vec::new();
    for tier in &cfg.tiers {
        let mut entries = Vec::new();
        for e in &tier.entries {
            let id = e.endpoint_id.clone();
            let healthy = balancer.health.is_healthy(&id);
            let state_str = if healthy { "healthy" } else { "cooling" };
            entries.push(json!({"provider": e.provider, "model": e.model, "healthy": healthy, "state": state_str, "is_default": e.is_default, "weight": e.weight}));
        }
        tiers_out.push(json!({"name": tier.name, "strict": tier.strict, "entries": entries}));
    }
    (StatusCode::OK, Json(json!({"tiers": tiers_out}))).into_response()
}

async fn handle_metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = check_auth(&state, &headers) { return resp; }
    let body = state.metrics.to_prometheus();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/plain; version=0.0.4"));
    (StatusCode::OK, headers, body).into_response()
}

async fn handle_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = check_auth(&state, &headers) { return resp; }
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

// --- Tracker admin endpoints ------------------------------------------------

#[derive(serde::Deserialize)]
struct TrackQuery {
    limit: Option<usize>,
    provider: Option<String>,
}

async fn handle_admin_track(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TrackQuery>,
) -> Response {
    if let Some(resp) = check_auth(&state, &headers) { return resp; }
    let mut entries = state.tracker.read_all();
    // newest first
    entries.reverse();
    if let Some(p) = &q.provider {
        entries.retain(|e| e.provider == *p);
    }
    let total = entries.len();
    let limit = q.limit.unwrap_or(100).max(1);
    let page: Vec<serde_json::Value> = entries
        .into_iter()
        .take(limit)
        .map(|e| {
            json!({
                "timestamp": e.timestamp,
                "provider": e.provider,
                "model": e.model,
                "latency_ms": e.latency_ms,
                "error_code": e.error_code,
                "error_string": error_string_for_code(e.error_code),
                "tier": e.tier,
                "is_success": e.is_success,
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({ "entries": page, "total": total }))).into_response()
}

async fn handle_admin_track_codes(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = check_auth(&state, &headers) { return resp; }
    let map: serde_json::Map<String, serde_json::Value> = code_map()
        .into_iter()
        .map(|(c, s)| (c.to_string(), json!(s)))
        .collect();
    (StatusCode::OK, Json(json!({ "codes": map }))).into_response()
}

// --- Tier creation endpoint (used by the wizard) ----------------------------

#[derive(serde::Deserialize)]
struct TierEntryInput {
    provider: String,
    model: String,
    #[serde(default = "default_weight_one")]
    weight: u32,
}

fn default_weight_one() -> u32 { 1 }

#[derive(serde::Deserialize)]
struct TierCreateInput {
    name: String,
    #[serde(default)]
    entries: Vec<TierEntryInput>,
}

/// Validate a custom tier name: non-empty, `[A-Za-z0-9_-]+`.
fn is_valid_tier_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

async fn handle_admin_create_tier(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TierCreateInput>,
) -> Response {
    if let Some(resp) = check_auth(&state, &headers) { return resp; }
    if !is_valid_tier_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid tier name; use [A-Za-z0-9_-]+" })),
        )
            .into_response();
    }
    if body.entries.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "tier must have at least one entry" })),
        )
            .into_response();
    }
    // Dedup entries by (provider, model); first occurrence wins and becomes default.
    let mut seen = std::collections::HashSet::new();
    let mut model_entries: Vec<xrouter_core::tier::ModelEntry> = Vec::new();
    for e in body.entries {
        let key = format!("{}:{}", e.provider, e.model);
        if seen.insert(key) {
            let is_default = model_entries.is_empty();
            model_entries.push(xrouter_core::tier::ModelEntry {
                provider: e.provider.clone(),
                model: e.model.clone(),
                is_default,
                weight: e.weight.max(1),
                endpoint_id: xrouter_core::EndpointId::new(&e.provider, &e.model),
            });
        }
    }
    let tier = xrouter_core::Tier {
        name: body.name.clone(),
        strict: true,
        default_entry: 0,
        entries: model_entries,
    };
    // Persist via config save, then update the live (arc-swapped) config.
    let mut cfg = state.get_config().as_ref().clone();
    if let Some(pos) = cfg.tiers.iter().position(|t| t.name == tier.name) {
        cfg.tiers[pos] = tier.clone();
    } else {
        cfg.tiers.push(tier.clone());
    }
    cfg.finalize();
    if let Err(e) = xrouter_config::save(&cfg) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    state.config.store(Arc::new(cfg));
    (
        StatusCode::OK,
        Json(json!({ "status": "created", "tier": tier.name })),
    )
        .into_response()
}

async fn handle_reload(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = check_auth(&state, &headers) { return resp; }
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

/// Record a single tracked request (no-op when tracking is disabled).
fn record_track(
    tracker: &Tracker,
    tier: &str,
    provider: &str,
    model: &str,
    success: bool,
    code: u16,
    latency_ms: u64,
) {
    tracker.record(track::TrackEntry {
        error_code: code,
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        provider: provider.to_string(),
        model: model.to_string(),
        tier: tier.to_string(),
        latency_ms,
        is_success: success,
    });
}

/// Detect a provider-specific quota signal in a response body. Used to map
/// non-429 quota responses to the 429 code for storage. NOTE: this alone does
/// NOT trigger a key ban — banning is strictly on HTTP 429 (see below).
fn body_is_quota(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("quota")
        || lower.contains("resource_exhausted")
        || lower.contains("insufficient_quota")
        || lower.contains("billing_quota")
        || lower.contains("rate_limit_exceeded")
}

/// Shared auth check used by both chat endpoints and `/admin/*`.
/// Returns `Some(response)` when authentication fails.
fn check_auth(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let cfg = state.get_config();
    if let Some(token) = cfg.settings.api_token.clone() {
        if !token.is_empty() {
            let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
            if auth != format!("Bearer {}", token) {
                return Some((StatusCode::UNAUTHORIZED, Json(json!({"error":{"type":"authentication_error","message":"invalid token"}}))).into_response());
            }
        }
    }
    None
}

async fn route_openai_request(state: &AppState, body: Value, headers: HeaderMap, is_anthropic_ingress: bool) -> Response {
    state.metrics.inc_requests();
    let start = std::time::Instant::now();
    // auth check
    if let Some(resp) = check_auth(state, &headers) {
        record_track(&state.tracker, "", "", "", false, 401, start.elapsed().as_millis() as u64);
        return resp;
    }

    let tier_name = match extract_tier_name(&body) {
        Some(n) => n,
        None => {
            let err = if is_anthropic_ingress {
                json!({"type":"error","error":{"type":"invalid_request_error","message":"missing model field"}})
            } else {
                json!({"error":{"type":"invalid_request_error","message":"missing model field"}})
            };
            state.metrics.inc_error();
            record_track(&state.tracker, "", "", "", false, 400, start.elapsed().as_millis() as u64);
            return (StatusCode::BAD_REQUEST, Json(err)).into_response();
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
            record_track(&state.tracker, &tier_name, "", "", false, 404, start.elapsed().as_millis() as u64);
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
    // Per-provider "key" count used to bound retry attempts. For device
    // providers this is the number of configured device accounts (round-robin
    // candidates); otherwise the API-key count from the balancer.
    let device_counts: std::collections::HashMap<String, usize> = if let Some(store) = state.device_store.load_full() {
        let s = store.lock().await;
        let mut m = std::collections::HashMap::new();
        for (prov, pcfg) in cfg.providers.iter() {
            if is_device_kind(&pcfg.kind) {
                m.insert(prov.clone(), s.count_for(device_provider_name(&pcfg.kind)));
            }
        }
        m
    } else {
        std::collections::HashMap::new()
    };
    let keys_per_provider = candidates.iter().map(|e| {
        device_counts.get(&e.provider).copied().unwrap_or_else(|| state.balancer.keys_len(&e.provider))
    }).max().unwrap_or(1).max(1);
    let max_attempts = std::cmp::min(candidates.len() * keys_per_provider, 6).max(1);
    let mut last_error: Option<(u16, String)> = None;
    let mut last_provider: String = String::new();
    let mut last_model: String = String::new();
    let mut attempt = 0usize;

    for entry in candidates.iter().take(max_attempts) {
        last_provider = entry.provider.clone();
        last_model = entry.model.clone();
        let pcfg = match cfg.providers.get(&entry.provider) {
            Some(p) => p.clone(),
            None => continue,
        };
        if !pcfg.enabled { continue; }

        let is_device = is_device_kind(&pcfg.kind);
        // For device providers, pick the next account round-robin, refresh its
        // token if needed, and use the access token as the bearer credential.
        // For key-based providers, use the balancer's round-robin key ring.
        let (key, device_account) = if is_device {
            match state.device_store.load_full() {
                Some(store) => {
                    let mut s = store.lock().await;
                    let name = device_provider_name(&pcfg.kind);
                    let mut acct = match s.next_account(name) {
                        Some(a) => a,
                        None => {
                            last_error = Some((401, format!("no device accounts for {}", name)));
                            state.metrics.inc_error();
                            continue;
                        }
                    };
                    if let Err(e) = xrouter_auth::refresh_if_needed(&mut acct).await {
                        warn!("device token refresh failed for {}: {}", name, e);
                    }
                    let _ = s.save();
                    (xrouter_core::ApiKey(acct.access_token.clone()), Some(acct))
                }
                None => {
                    last_error = Some((401, "device store unavailable".to_string()));
                    state.metrics.inc_error();
                    continue;
                }
            }
        } else {
            let key_opt = state.balancer.next_key(&entry.provider);
            let (key, _idx) = match key_opt {
                Some(k) => k,
                None => {
                    last_error = Some((401, "no valid keys".to_string()));
                    state.metrics.inc_error();
                    continue;
                }
            };
            (key, None)
        };
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

        let endpoint_id = entry.endpoint_id.clone();
        // If this endpoint is in its half-open probe window, claim the single
        // allowed probe so concurrent requests don't all hit it at once.
        if state.balancer.health.is_half_open(&endpoint_id) {
            state.balancer.health.mark_probing(&endpoint_id);
        }
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
            record_track(&state.tracker, &tier_name, &entry.provider, &entry.model, true, 200, start.elapsed().as_millis() as u64);
            tracing::info!(tier=tier_name, provider=entry.provider, model=entry.model, latency_ms=start.elapsed().as_millis() as u64, status=upstream.status, "request success");
            if streaming {
                let need_translate = is_anthropic_ingress != upstream_is_anthropic;
                let headers_out = build_sse_headers();
                if need_translate {
                    // Real chunked streaming translation: map each upstream
                    // chunk through a stateful translator so we never buffer
                    // the whole SSE stream and never lose tool calls.
                    let translator = StreamTranslator::new(!upstream_is_anthropic, is_anthropic_ingress);
                    let inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
                        Box::pin(upstream.response.bytes_stream());
                    let stream = futures::stream::unfold((inner, translator, false), |(mut inner, mut translator, flushed)| async move {
                        if flushed { return None; }
                        match inner.as_mut().next().await {
                            Some(result) => {
                                let chunk = result.unwrap_or_default();
                                let translated = translator.push(&chunk);
                                Some((Ok::<_, Infallible>(Bytes::from(translated)), (inner, translator, false)))
                            }
                            None => {
                                let translated = translator.flush();
                                Some((Ok::<_, Infallible>(Bytes::from(translated)), (inner, translator, true)))
                            }
                        }
                    });
                    return (StatusCode::OK, headers_out, Body::from_stream(stream)).into_response();
                } else {
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
            if is_device {
                // Token rejected: refresh and retry with the next account.
                if let Some(mut acct) = device_account {
                    if let Err(e) = xrouter_auth::refresh_if_needed(&mut acct).await {
                        warn!("device token refresh after 401 failed: {}", e);
                    }
                    if let Some(store) = state.device_store.load_full() {
                        let mut s = store.lock().await;
                        s.add_account(acct);
                        let _ = s.save();
                    }
                }
                state.metrics.inc_retries();
                attempt += 1;
                continue;
            }
            state.balancer.health.mark_key_dead(key.expose());
            let body_bytes = upstream.response.bytes().await.unwrap_or_default();
            last_error = Some((upstream.status, String::from_utf8_lossy(&body_bytes).to_string()));
            state.metrics.inc_retries();
            attempt += 1;
            continue;
        } else if upstream.status == 429 {
            if is_device {
                // Quota on a device account: refresh token and retry.
                if let Some(mut acct) = device_account {
                    if let Err(e) = xrouter_auth::refresh_if_needed(&mut acct).await {
                        warn!("device token refresh after 429 failed: {}", e);
                    }
                    if let Some(store) = state.device_store.load_full() {
                        let mut s = store.lock().await;
                        s.add_account(acct);
                        let _ = s.save();
                    }
                }
                let body_bytes = upstream.response.bytes().await.unwrap_or_default();
                last_error = Some((upstream.status, String::from_utf8_lossy(&body_bytes).to_string()));
                state.metrics.inc_retries();
                attempt += 1;
                continue;
            }
            // Quota error: ban this key for the provider's configured duration.
            // Strictly only on HTTP 429 — never on 500/502/400/timeout/etc.
            state.balancer.ban_key(&entry.provider, key.expose(), pcfg.quota_ban_secs);
            tracing::warn!(
                provider = entry.provider,
                model = entry.model,
                ban_secs = pcfg.quota_ban_secs,
                "quota error (429) — banning key"
            );
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
    let code = if let Some((c, body)) = &last_error {
        if *c == 429 || body_is_quota(body) { 429 } else { error_code_for_status(*c) }
    } else { 503 };
    record_track(&state.tracker, &tier_name, &last_provider, &last_model, false, code, start.elapsed().as_millis() as u64);
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

/// Stateful, chunk-wise SSE translator. Buffers partial lines across network
/// chunks and carries per-stream translation state (tool-call blocks, message
/// start, finish reason) so cross-protocol streaming never loses data.
struct StreamTranslator {
    upstream_is_openai: bool,
    ingress_is_anthropic: bool,
    pending_event: Option<String>,
    line_buf: String,
    oa_state: translate::OaToAnthStreamState,
    anth_state: translate::AnthToOaStreamState,
}

impl StreamTranslator {
    fn new(upstream_is_openai: bool, ingress_is_anthropic: bool) -> Self {
        Self {
            upstream_is_openai,
            ingress_is_anthropic,
            pending_event: None,
            line_buf: String::new(),
            oa_state: Default::default(),
            anth_state: Default::default(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> String {
        let text = String::from_utf8_lossy(chunk);
        self.line_buf.push_str(&text);
        let mut out = String::new();
        while let Some(pos) = self.line_buf.find('\n') {
            let mut line = self.line_buf[..pos].to_string();
            self.line_buf.drain(..=pos);
            if line.ends_with('\r') { line.pop(); }
            self.process_line(&line, &mut out);
        }
        out
    }

    fn flush(&mut self) -> String {
        let mut out = String::new();
        if !self.line_buf.is_empty() {
            let line = self.line_buf.clone();
            self.line_buf.clear();
            self.process_line(&line, &mut out);
        }
        // Ensure a clean terminal event if the upstream closed without one.
        if self.upstream_is_openai && self.ingress_is_anthropic && !self.oa_state.finished {
            out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
            self.oa_state.finished = true;
        }
        if !self.upstream_is_openai && self.ingress_is_anthropic && !self.anth_state.finished {
            out.push_str("data: [DONE]\n\n");
            self.anth_state.finished = true;
        }
        out
    }

    fn process_line(&mut self, line: &str, out: &mut String) {
        if line.starts_with("data: ") {
            let data = line.trim_start_matches("data: ").trim();
            if data == "[DONE]" {
                if self.ingress_is_anthropic {
                    out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
                    self.oa_state.finished = true;
                } else {
                    out.push_str("data: [DONE]\n\n");
                    self.anth_state.finished = true;
                }
                self.pending_event = None;
                return;
            }
            if self.upstream_is_openai && self.ingress_is_anthropic {
                for c in translate::translate_sse_openai_to_anthropic_chunk_st(&mut self.oa_state, data) {
                    out.push_str(&c);
                    out.push_str("\n\n");
                }
            } else if !self.upstream_is_openai && !self.ingress_is_anthropic {
                let ev = self.pending_event.take().unwrap_or_else(|| "content_block_delta".to_string());
                if let Some(translated) = translate::translate_sse_anthropic_to_openai_chunk_st(&mut self.anth_state, &ev, data) {
                    out.push_str(&translated);
                }
            } else {
                out.push_str(line);
                out.push('\n');
                out.push('\n');
            }
        } else if line.starts_with("event:") {
            let ev = line.trim_start_matches("event:").trim().to_string();
            if !self.upstream_is_openai && !self.ingress_is_anthropic {
                self.pending_event = Some(ev);
            } else if self.ingress_is_anthropic && self.upstream_is_openai {
                // openai->anthropic: events are synthesized by the translator; ignore upstream events
            } else {
                out.push_str(line);
                out.push('\n');
            }
        } else if line.is_empty() {
            // skip
        } else if line.starts_with(":") {
            out.push_str(line);
            out.push('\n');
        }
    }
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
    let val: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error":{"type":"invalid_request_error","message": e.to_string()}}))).into_response(),
    };
    route_openai_request(&state, val, headers, false).await
}

async fn handle_messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let val: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"type":"error","error":{"type":"invalid_request_error","message": e.to_string()}}))).into_response(),
    };
    route_openai_request(&state, val, headers, true).await
}

// --- Image generation route (lazy; off the hot chat path) -------------------
//
// Mirrors the chat routing semantics (strict tier lookup, RR + quota-ban on
// 429, retry within tier, health) but posts to `{base}/images/generations`
// instead of `/chat/completions`. No streaming, no cross-protocol translation.

async fn handle_images_generations(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let val: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error":{"type":"invalid_request_error","message": e.to_string()}}))).into_response(),
    };

    state.metrics.inc_requests();
    let start = std::time::Instant::now();

    // auth check
    if let Some(resp) = check_auth(&state, &headers) {
        record_track(&state.tracker, "", "", "", false, 401, start.elapsed().as_millis() as u64);
        return resp;
    }

    // prompt is required for image generation
    if val.get("prompt").and_then(|p| p.as_str()).map(|s| s.trim().is_empty()).unwrap_or(true) {
        state.metrics.inc_error();
        record_track(&state.tracker, "", "", "", false, 400, start.elapsed().as_millis() as u64);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"type":"invalid_request_error","message":"missing or empty 'prompt' field"}})),
        )
            .into_response();
    }

    let tier_name = match extract_tier_name(&val) {
        Some(n) => n,
        None => {
            state.metrics.inc_error();
            record_track(&state.tracker, "", "", "", false, 400, start.elapsed().as_millis() as u64);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"type":"invalid_request_error","message":"missing model field"}})),
            )
                .into_response();
        }
    };

    let cfg = state.get_config();
    let tier = match cfg.tiers.iter().find(|t| t.name == tier_name) {
        Some(t) => t.clone(),
        None => {
            let available = cfg.tiers.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
            state.metrics.inc_error();
            record_track(&state.tracker, &tier_name, "", "", false, 404, start.elapsed().as_millis() as u64);
            return (StatusCode::NOT_FOUND, Json(xrouter_core::unknown_tier_body(&tier_name, &available))).into_response();
        }
    };

    let candidates = state.balancer.candidates_ordered(&tier);
    if candidates.is_empty() {
        state.metrics.inc_tier_exhausted();
        let body_json = xrouter_core::tier_exhausted_body(&tier_name);
        return (StatusCode::SERVICE_UNAVAILABLE, Json(body_json)).into_response();
    }
    // Per-provider "key" count used to bound retry attempts. For device
    // providers this is the number of configured device accounts; otherwise the
    // API-key count from the balancer.
    let device_counts: std::collections::HashMap<String, usize> = if let Some(store) = state.device_store.load_full() {
        let s = store.lock().await;
        let mut m = std::collections::HashMap::new();
        for (prov, pcfg) in cfg.providers.iter() {
            if is_device_kind(&pcfg.kind) {
                m.insert(prov.clone(), s.count_for(device_provider_name(&pcfg.kind)));
            }
        }
        m
    } else {
        std::collections::HashMap::new()
    };
    let keys_per_provider = candidates.iter().map(|e| {
        device_counts.get(&e.provider).copied().unwrap_or_else(|| state.balancer.keys_len(&e.provider))
    }).max().unwrap_or(1).max(1);
    let max_attempts = std::cmp::min(candidates.len() * keys_per_provider, 6).max(1);
    let mut last_error: Option<(u16, String)> = None;
    let mut last_provider: String = String::new();
    let mut last_model: String = String::new();
    let mut attempt = 0usize;

    for entry in candidates.iter().take(max_attempts) {
        last_provider = entry.provider.clone();
        last_model = entry.model.clone();
        let pcfg = match cfg.providers.get(&entry.provider) {
            Some(p) => p.clone(),
            None => continue,
        };
        if !pcfg.enabled { continue; }

        let is_device = is_device_kind(&pcfg.kind);
        let (key, device_account) = if is_device {
            match state.device_store.load_full() {
                Some(store) => {
                    let mut s = store.lock().await;
                    let name = device_provider_name(&pcfg.kind);
                    let mut acct = match s.next_account(name) {
                        Some(a) => a,
                        None => {
                            last_error = Some((401, format!("no device accounts for {}", name)));
                            state.metrics.inc_error();
                            continue;
                        }
                    };
                    if let Err(e) = xrouter_auth::refresh_if_needed(&mut acct).await {
                        warn!("device token refresh failed for {}: {}", name, e);
                    }
                    let _ = s.save();
                    (xrouter_core::ApiKey(acct.access_token.clone()), Some(acct))
                }
                None => {
                    last_error = Some((401, "device store unavailable".to_string()));
                    state.metrics.inc_error();
                    continue;
                }
            }
        } else {
            let key_opt = state.balancer.next_key(&entry.provider);
            let (key, _idx) = match key_opt {
                Some(k) => k,
                None => {
                    last_error = Some((401, "no valid keys".to_string()));
                    state.metrics.inc_error();
                    continue;
                }
            };
            (key, None)
        };
        // Translate body: keep prompt/n/size/response_format passthrough, just
        // set the upstream model. Image providers are openai-compat only.
        let mut translated = val.clone();
        translated["model"] = Value::String(entry.model.clone());

        let endpoint_id = entry.endpoint_id.clone();
        if state.balancer.health.is_half_open(&endpoint_id) {
            state.balancer.health.mark_probing(&endpoint_id);
        }
        let ctx = RequestCtx {
            provider: entry.provider.clone(),
            base_url: pcfg.base_url.clone(),
            model: entry.model.clone(),
            api_key: key.clone(),
            body: translated.clone(),
            stream: false,
        };

        let adapter: Box<dyn xrouter_providers::Provider> = xrouter_providers::make_provider(&pcfg.kind, pcfg.base_url.clone(), state.client.clone());

        let res = tokio::time::timeout(Duration::from_secs(15), adapter.send_images(&ctx)).await;
        let upstream = match res {
            Ok(Ok(u)) => u,
            Ok(Err(e)) => {
                warn!("image upstream error {}: {}", entry.provider, e);
                state.balancer.health.mark_failure(&endpoint_id);
                state.metrics.inc_retries();
                last_error = Some((502, e.to_string()));
                attempt += 1;
                if attempt >= max_attempts { break; }
                continue;
            }
            Err(_) => {
                warn!("image timeout {}", entry.provider);
                state.balancer.health.mark_failure(&endpoint_id);
                state.metrics.inc_timeouts();
                state.metrics.inc_retries();
                last_error = Some((504, "upstream timeout".to_string()));
                attempt += 1;
                if attempt >= max_attempts { break; }
                continue;
            }
        };

        if upstream.status >= 200 && upstream.status < 300 {
            state.balancer.health.mark_success(&endpoint_id);
            state.metrics.inc_success();
            record_track(&state.tracker, &tier_name, &entry.provider, &entry.model, true, 200, start.elapsed().as_millis() as u64);
            tracing::info!(tier=tier_name, provider=entry.provider, model=entry.model, latency_ms=start.elapsed().as_millis() as u64, status=upstream.status, "image request success");
            let body_bytes = upstream.response.bytes().await.unwrap_or_default();
            let mut headers_out = HeaderMap::new();
            headers_out.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
            return (StatusCode::OK, headers_out, Body::from(body_bytes)).into_response();
        } else if upstream.status == 401 || upstream.status == 403 {
            if is_device {
                if let Some(mut acct) = device_account {
                    if let Err(e) = xrouter_auth::refresh_if_needed(&mut acct).await {
                        warn!("device token refresh after 401 failed: {}", e);
                    }
                    if let Some(store) = state.device_store.load_full() {
                        let mut s = store.lock().await;
                        s.add_account(acct);
                        let _ = s.save();
                    }
                }
                state.metrics.inc_retries();
                attempt += 1;
                continue;
            }
            state.balancer.health.mark_key_dead(key.expose());
            let body_bytes = upstream.response.bytes().await.unwrap_or_default();
            last_error = Some((upstream.status, String::from_utf8_lossy(&body_bytes).to_string()));
            state.metrics.inc_retries();
            attempt += 1;
            continue;
        } else if upstream.status == 429 {
            if is_device {
                if let Some(mut acct) = device_account {
                    if let Err(e) = xrouter_auth::refresh_if_needed(&mut acct).await {
                        warn!("device token refresh after 429 failed: {}", e);
                    }
                    if let Some(store) = state.device_store.load_full() {
                        let mut s = store.lock().await;
                        s.add_account(acct);
                        let _ = s.save();
                    }
                }
                let body_bytes = upstream.response.bytes().await.unwrap_or_default();
                last_error = Some((upstream.status, String::from_utf8_lossy(&body_bytes).to_string()));
                state.metrics.inc_retries();
                attempt += 1;
                continue;
            }
            state.balancer.ban_key(&entry.provider, key.expose(), pcfg.quota_ban_secs);
            tracing::warn!(
                provider = entry.provider,
                model = entry.model,
                ban_secs = pcfg.quota_ban_secs,
                "image quota error (429) — banning key"
            );
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

    state.metrics.inc_tier_exhausted();
    state.metrics.inc_error();
    let code = if let Some((c, body)) = &last_error {
        if *c == 429 || body_is_quota(body) { 429 } else { error_code_for_status(*c) }
    } else { 503 };
    record_track(&state.tracker, &tier_name, &last_provider, &last_model, false, code, start.elapsed().as_millis() as u64);
    let status = if let Some((code, _)) = last_error {
        if code == 401 || code == 403 { StatusCode::UNAUTHORIZED } else if code == 504 { StatusCode::GATEWAY_TIMEOUT } else if code == 429 { StatusCode::TOO_MANY_REQUESTS } else { StatusCode::SERVICE_UNAVAILABLE }
    } else { StatusCode::SERVICE_UNAVAILABLE };
    tracing::warn!(tier=tier_name, latency_ms=start.elapsed().as_millis() as u64, "image tier exhausted");
    (status, Json(xrouter_core::tier_exhausted_body(&tier_name))).into_response()
}

pub async fn run_server(addr: String, state: AppState) -> anyhow::Result<()> {
    // Populate the model cache at startup so /admin/models serves from cache.
    state.refresh_model_cache().await;
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("listening on {}", addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve when a shutdown signal (SIGINT / SIGTERM) is received.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => { let _ = sig.recv().await; }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

// Fallback non-unix run
pub async fn run_server_simple(addr: String, state: AppState) -> anyhow::Result<()> {
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
