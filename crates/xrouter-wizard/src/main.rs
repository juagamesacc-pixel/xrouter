/// Minimal HTTP server that serves the self-contained wizard HTML templates
/// produced by the `xrouter-wizard` library crate, plus a small JSON API that
/// proxies model discovery, persists the wizard's configuration, and drives the
/// device-login (OAuth device flow) for kiro / antigravity providers.
///
/// The `xrouter` CLI spawns this binary (as `xrouter-wizard-server`) on :3001
/// for the `xrouter wizard --web` flow. It depends only on `std` +
/// `serde_json`/`toml` plus `xrouter-auth` (for the device flow), so it stays
/// out of the main server hot path.
///
/// Routes:
///   GET /                     -> WIZARD_HTML
///   GET /metrics              -> METRICS_HTML
///   GET /api/config           -> current config as JSON (builtins merged)
///   POST /api/config          -> persist config (JSON body) to config.toml
///   GET /api/models/fetch?provider=X
///                             -> server-side proxy to the provider's /models
///   GET /api/device/accounts  -> list stored device accounts
///   POST /api/device/login    -> begin device login (kiro: code+uri; antigravity: auth_url)
///   POST /api/device/poll     -> poll kiro until approved, then persist account
///   GET  /oauth/callback      -> antigravity (Google) loopback PKCE callback
///   anything else             -> 404

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use xrouter_auth;

/// Pending kiro device logins, keyed by `device_code`. The full
/// [`xrouter_auth::DeviceLoginInit`] (which carries the dynamically-registered
/// `client_id`/`client_secret` needed to poll the token endpoint) is kept
/// server-side so it never reaches the browser.
static KIRO_PENDING: OnceLock<Mutex<HashMap<String, xrouter_auth::DeviceLoginInit>>> =
    OnceLock::new();

/// Pending antigravity (Google) login: the full PKCE [`GoogleLoginInit`] (code
/// verifier + redirect_uri + state) needed to complete the loopback callback
/// exchange. Only one in flight at a time.
static GOOGLE_PENDING: OnceLock<Mutex<Option<xrouter_auth::GoogleLoginInit>>> = OnceLock::new();

/// Redirect URI the wizard's own loopback callback route listens on.
const GOOGLE_REDIRECT_URI: &str = "http://localhost:3001/oauth/callback";

// ── Config model (mirrors crates/xrouter-config schema) ──────────────────────

#[derive(Serialize, Deserialize, Default, Clone)]
struct Settings {
    #[serde(default)]
    default_tier: Option<String>,
    #[serde(default)]
    api_token: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ProviderConfig {
    #[serde(default = "default_kind")]
    kind: String,
    base_url: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default = "default_quota_ban_secs")]
    quota_ban_secs: u64,
    #[serde(default)]
    accounts: Vec<Value>,
}

fn default_kind() -> String {
    "openai-compat".into()
}
fn default_true() -> bool {
    true
}
fn default_quota_ban_secs() -> u64 {
    300
}

#[derive(Serialize, Deserialize, Clone)]
struct ModelEntry {
    provider: String,
    model: String,
    #[serde(default)]
    is_default: bool,
    #[serde(default = "default_weight")]
    weight: u32,
    #[serde(default)]
    endpoint_id: String,
}
fn default_weight() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Clone)]
struct Tier {
    name: String,
    #[serde(default = "default_true")]
    strict: bool,
    #[serde(default)]
    default_entry: usize,
    entries: Vec<ModelEntry>,
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    tiers: Vec<Tier>,
}

// ── Config path + builtins ───────────────────────────────────────────────────

fn config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("XROUTER_CONFIG") {
        return p.into();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut p = std::path::PathBuf::from(home);
    p.push(".config");
    p.push("xrouter");
    p.push("config.toml");
    p
}

fn builtin_providers() -> Vec<(String, ProviderConfig)> {
    vec![
        (
            "opencode-zen".into(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: "https://opencode.ai/zen/v1".into(),
                enabled: true,
                keys: vec![],
                quota_ban_secs: 300,
                accounts: vec![],
            },
        ),
        (
            "openrouter".into(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                enabled: true,
                keys: vec![],
                quota_ban_secs: 300,
                accounts: vec![],
            },
        ),
        (
            "together-image".into(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: "https://api.together.ai/v1".into(),
                enabled: true,
                keys: vec![],
                quota_ban_secs: 300,
                accounts: vec![],
            },
        ),
    ]
}

/// Default tiers that ship with xrouter. These are merged into the loaded
/// config so the wizard always shows `big-pickle` (opencode-zen) and `images`
/// (together-image) without the user having to create them.
fn builtin_tiers() -> Vec<Tier> {
    vec![
        Tier {
            name: "big-pickle".into(),
            strict: true,
            default_entry: 0,
            entries: vec![ModelEntry {
                provider: "opencode-zen".into(),
                model: "big-pickle".into(),
                is_default: true,
                weight: 1,
                endpoint_id: String::new(),
            }],
        },
        Tier {
            name: "images".into(),
            strict: true,
            default_entry: 0,
            entries: vec![ModelEntry {
                provider: "together-image".into(),
                model: "black-forest-labs/FLUX.1-schnell".into(),
                is_default: true,
                weight: 1,
                endpoint_id: String::new(),
            }],
        },
    ]
}

fn load_config_from(path: &std::path::Path) -> Config {
    let mut cfg: Config = match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    };
    for (id, p) in builtin_providers() {
        cfg.providers.entry(id).or_insert(p);
    }
    for t in builtin_tiers() {
        if !cfg.tiers.iter().any(|x| x.name == t.name) {
            cfg.tiers.push(t);
        }
    }
    cfg
}

fn load_config() -> Config {
    load_config_from(&config_path())
}

fn save_config_to(cfg: &Config, path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let toml = toml::to_string_pretty(cfg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Atomic write: temp file + rename, with 0600 perms (same as xrouter-config).
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &toml)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&tmp, perm);
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perm);
    }
    Ok(())
}

fn save_config(cfg: &Config) -> std::io::Result<()> {
    save_config_to(cfg, &config_path())
}

/// Generate a secure router API key: `xr_` + 32 hex chars (16 random bytes).
fn gen_router_key() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let n: u128 = rng.random();
    format!("xr_{:032x}", n)
}

/// Re-read the just-written config from disk and compare it against what was
/// requested. Returns the verification JSON payload on a match, or an error
/// string describing the first mismatch. This guards against the
/// "UI claims success but the file was never actually written" class of bug.
fn verify_saved_config(requested: &Config, path: &std::path::Path) -> Result<Value, String> {
    let on_disk_str = std::fs::read_to_string(path)
        .map_err(|e| format!("could not re-read saved config: {}", e))?;
    let on_disk: Config = toml::from_str(&on_disk_str)
        .map_err(|e| format!("saved config is not valid toml: {}", e))?;

    // Providers: id, base_url, keys count, enabled.
    let mut providers = serde_json::Map::new();
    for (id, p) in &requested.providers {
        let saved = on_disk
            .providers
            .get(id)
            .ok_or_else(|| format!("provider '{}' missing after save", id))?;
        if saved.base_url != p.base_url {
            return Err(format!("provider '{}' base_url mismatch after save", id));
        }
        if saved.enabled != p.enabled {
            return Err(format!("provider '{}' enabled flag mismatch after save", id));
        }
        if saved.keys.len() != p.keys.len() {
            return Err(format!(
                "provider '{}' key count mismatch after save (saved {}, expected {})",
                id,
                saved.keys.len(),
                p.keys.len()
            ));
        }
        providers.insert(
            id.clone(),
            serde_json::json!({
                "keys": saved.keys.len(),
                "enabled": saved.enabled,
                "base_url": saved.base_url,
            }),
        );
    }

    // Tiers: names.
    let mut req_tier_names: Vec<&String> = requested.tiers.iter().map(|t| &t.name).collect();
    req_tier_names.sort();
    let mut saved_tier_names: Vec<&String> = on_disk.tiers.iter().map(|t| &t.name).collect();
    saved_tier_names.sort();
    if req_tier_names != saved_tier_names {
        return Err(format!(
            "tier names mismatch after save (saved: {:?}, expected: {:?})",
            saved_tier_names, req_tier_names
        ));
    }

    Ok(serde_json::json!({
        "ok": true,
        "verified": true,
        "providers": providers,
        "tiers": req_tier_names,
    }))
}

/// Handle `POST /api/config`: validate, persist atomically, then re-read the
/// file from disk and verify the save actually landed. Returns (json, status).
fn post_config(body: &str, path: &std::path::Path) -> (String, String) {
    match serde_json::from_str::<Config>(body) {
        Ok(cfg) => {
            if let Some(err) = validate_config(&cfg) {
                (json_error(&err), "400 Bad Request".into())
            } else {
                match save_config_to(&cfg, path) {
                    Ok(()) => match verify_saved_config(&cfg, path) {
                        Ok(verified) => (verified.to_string(), "200 OK".into()),
                        Err(e) => (
                            serde_json::json!({ "ok": false, "error": format!("save verification failed: {}", e) }).to_string(),
                            "500 Internal Server Error".into(),
                        ),
                    },
                    Err(e) => (
                        serde_json::json!({ "ok": false, "error": format!("save verification failed: write error: {}", e) }).to_string(),
                        "500 Internal Server Error".into(),
                    ),
                }
            }
        }
        Err(e) => (
            json_error(&format!("invalid config json: {}", e)),
            "400 Bad Request".into(),
        ),
    }
}

/// Validate a config before persisting. Tier names must match
/// `^[A-Za-z0-9_-]+$` and every entry needs a non-empty provider + model.
/// This guards the Models-tab "create/append tier" flow so only well-formed
/// tiers are written to disk.
fn validate_config(cfg: &Config) -> Option<String> {
    for t in &cfg.tiers {
        if t.name.is_empty()
            || !t
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Some(format!(
                "invalid tier name '{}': only A-Za-z0-9_- allowed",
                t.name
            ));
        }
        for e in &t.entries {
            if e.provider.trim().is_empty() || e.model.trim().is_empty() {
                return Some(format!(
                    "tier '{}' has an entry with an empty provider or model",
                    t.name
                ));
            }
        }
    }
    None
}

fn json_error(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

fn url_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(h) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(h as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

// ── Model fetch proxy ────────────────────────────────────────────────────────

/// Decide whether a model is free using both pricing data (when the provider
/// returns `pricing.prompt`/`pricing.completion` equal to `"0.000000"`) and a
/// name heuristic (`-free`, `[free]`, `(free)`, `:free`, `opencode/big-pickle`).
fn is_free_model(id: &str, prompt: Option<&str>, completion: Option<&str>) -> bool {
    if let (Some(p), Some(c)) = (prompt, completion) {
        if p == "0.000000" && c == "0.000000" {
            return true;
        }
    }
    let n = id.to_lowercase();
    n.ends_with("-free")
        || n.contains("-free-")
        || n.contains("[free]")
        || n.contains("(free)")
        || n.contains(":free")
        || n.contains("opencode/big-pickle")
        || n == "big-pickle"
        || n.contains("big-pickle")
}

/// Proxies `GET {base_url}/models` for the given provider using its first key.
/// Returns (json_body, http_status). On any failure the body is
/// `{ "error": "..." }` (capturing curl's stderr) so the frontend can surface a
/// custom modal. On success the body is `{ "data": [ { "id", "free" }, ... ] }`.
fn fetch_models(provider: &str) -> (String, String) {
    let cfg = load_config();
    let p = match cfg.providers.get(provider) {
        Some(p) => p,
        None => {
            return (
                json_error(&format!("provider not found: {}", provider)),
                "404 Not Found".into(),
            )
        }
    };
    if p.keys.is_empty() {
        return (
            json_error(&format!("no api key configured for provider '{}'", provider)),
            "400 Bad Request".into(),
        );
    }
    let base = p.base_url.trim_end_matches('/');
    let url = format!("{}/models", base);

    // Rotate through every configured key until one successfully returns models.
    let mut last_err = String::from("no api keys available");
    for key in &p.keys {
        let out = Command::new("curl")
            .args([
                "-sS",
                "-m",
                "20",
                "-H",
                &format!("Authorization: Bearer {}", key),
                &url,
            ])
            .output();

        let out = match out {
            Ok(o) => o,
            Err(e) => {
                last_err = format!("failed to invoke curl: {}", e);
                continue;
            }
        };

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            last_err = if stderr.is_empty() {
                format!("curl exited with status {}", out.status)
            } else {
                stderr
            };
            continue;
        }

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        match serde_json::from_str::<Value>(&stdout) {
            Ok(v) => {
                // A provider error payload (no data) means this key didn't work —
                // rotate to the next key instead of surfacing a dead response.
                if v.get("error").is_some() && v.get("data").is_none() {
                    last_err = format!(
                        "provider error: {}",
                        v.get("error")
                            .and_then(|x| x.as_str())
                            .unwrap_or("unknown")
                    );
                    continue;
                }
                let data = v
                    .get("data")
                    .and_then(|d| d.as_array())
                    .cloned()
                    .unwrap_or_default();
                let models: Vec<Value> = data
                    .iter()
                    .filter_map(|m| {
                        let id = m
                            .get("id")
                            .and_then(|i| i.as_str())
                            .or_else(|| m.get("name").and_then(|n| n.as_str()))?;
                        let pricing = m.get("pricing");
                        let prompt = pricing
                            .and_then(|p| p.get("prompt"))
                            .and_then(|x| x.as_str());
                        let completion = pricing
                            .and_then(|p| p.get("completion"))
                            .and_then(|x| x.as_str());
                        let free = is_free_model(id, prompt, completion);
                        Some(serde_json::json!({ "id": id, "free": free }))
                    })
                    .collect();
                return (
                    serde_json::json!({ "data": models }).to_string(),
                    "200 OK".into(),
                );
            }
            Err(e) => {
                last_err = format!("invalid JSON from provider: {}", e);
                continue;
            }
        }
    }

    (json_error(&last_err), "502 Bad Gateway".into())
}

// ── Device login (kiro / antigravity) ─────────────────────────────────────────

/// Lazily-created multi-thread tokio runtime used to drive the async
/// `xrouter_auth` device-flow calls from this otherwise-synchronous server.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}

fn device_base_url(provider: &str) -> String {
    match provider {
        "kiro" => "https://api.kiro.dev/v1".to_string(),
        "antigravity" => "https://api.cline.bot/api/v1".to_string(),
        other => format!("https://{}.invalid/v1", other),
    }
}

/// POST /api/device/login {provider}
/// Begins a login flow for the provider and returns what the UI needs:
///   * `antigravity` (Google) — `{ auth_url }` (PKCE consent URL). The browser
///     opens it; the loopback `GET /oauth/callback` completes the exchange.
///   * `kiro` (AWS Builder ID) — `{ verification_uri, verification_uri_complete,
///     user_code, device_code }` for the device-code poll flow.
/// The account identity is derived automatically after the login completes — no
/// manual `account_id` is requested.
fn device_login(body: &str) -> (String, String) {
    let req: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (json_error(&format!("invalid json: {}", e)), "400 Bad Request".into()),
    };
    let provider = req
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if provider.is_empty() {
        return (
            json_error("provider is required"),
            "400 Bad Request".into(),
        );
    }
    let prov_name = xrouter_auth::normalize_provider(&provider).to_string();
    if !xrouter_auth::DEVICE_PROVIDERS.contains(&prov_name.as_str()) {
        return (
            json_error(&format!("unsupported device provider '{}'", provider)),
            "400 Bad Request".into(),
        );
    }

    if prov_name == "antigravity" {
        // PKCE browser flow: build the consent URL and stash the verifier
        // server-side for the /oauth/callback exchange.
        let init = xrouter_auth::build_google_auth_url(GOOGLE_REDIRECT_URI);
        *GOOGLE_PENDING
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(init.clone());
        let resp = serde_json::json!({
            "auth_url": init.auth_url,
            "provider": "antigravity",
        });
        return (resp.to_string(), "200 OK".into());
    }

    // kiro (and any future device-code provider): start the device flow.
    match rt().block_on(xrouter_auth::initiate_device_login(&prov_name, "", "")) {
        Ok(init) => {
            let device_code = init.device_code.clone();
            KIRO_PENDING
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap()
                .insert(device_code.clone(), init.clone());
            let resp = serde_json::json!({
                "verification_uri": init.verification_uri,
                "verification_uri_complete": init.verification_uri_complete,
                "user_code": init.user_code,
                "device_code": init.device_code,
                "provider": init.provider,
            });
            (resp.to_string(), "200 OK".into())
        }
        Err(e) => (
            json_error(&format!("device login init failed: {}", e)),
            "502 Bad Gateway".into(),
        ),
    }
}

/// POST /api/device/poll {provider, device_code}
/// Polls (blocking, up to the provider's timeout) until the user approves the
/// device login, then persists the account to the off-RAM device store and to
/// the config (metadata only, no secrets). Used by the kiro device-code flow;
/// antigravity completes via the loopback `GET /oauth/callback` instead.
fn device_poll(body: &str) -> (String, String) {
    let req: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (json_error(&format!("invalid json: {}", e)), "400 Bad Request".into()),
    };
    let provider = req
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let device_code = req
        .get("device_code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if provider.is_empty() || device_code.is_empty() {
        return (
            json_error("provider and device_code are required"),
            "400 Bad Request".into(),
        );
    }
    let prov_name = xrouter_auth::normalize_provider(&provider).to_string();
    if prov_name == "antigravity" {
        return (
            json_error("antigravity completes via the browser callback, not polling"),
            "400 Bad Request".into(),
        );
    }
    let init = match KIRO_PENDING
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .remove(&device_code)
    {
        Some(i) => i,
        None => {
            return (
                json_error("no pending device login for that device_code"),
                "400 Bad Request".into(),
            )
        }
    };
    match rt().block_on(xrouter_auth::poll_device_login(&init)) {
        Ok(acct) => {
            let mut store = xrouter_auth::DeviceStore::load()
                .unwrap_or_else(|_| xrouter_auth::DeviceStore::empty());
            store.add_account(acct.clone());
            if let Err(e) = store.save() {
                return (
                    json_error(&format!("failed to save device store: {}", e)),
                    "500 Internal Server Error".into(),
                );
            }
            save_device_account_to_config(&prov_name, &acct);
            let resp = serde_json::json!({
                "ok": true,
                "identity": acct.account_id,
                "account": {
                    "provider": acct.provider,
                    "account_id": acct.account_id,
                    "display_name": acct.display_name,
                }
            });
            (resp.to_string(), "200 OK".into())
        }
        Err(e) => (
            json_error(&format!("device login failed: {}", e)),
            "502 Bad Gateway".into(),
        ),
    }
}

/// GET /oauth/callback?code=...&state=...  (antigravity / Google loopback)
/// Completes the PKCE exchange using the server-side-stashed verifier, persists
/// the account, and returns a tiny HTML page telling the user they can close
/// the tab.
fn device_oauth_callback(query: &str) -> (String, String) {
    let params: HashMap<String, String> = query
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), url_decode(v)))
        })
        .collect();
    let code = params.get("code").cloned().unwrap_or_default();
    let err = params.get("error").cloned().unwrap_or_default();
    let pending = GOOGLE_PENDING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    let pending = match pending {
        Some(p) => p,
        None => {
            return (
                html_page("Login failed", "No pending Google login session was found. Restart the login from the wizard."),
                "400 Bad Request".into(),
            )
        }
    };
    if !err.is_empty() {
        return (
            html_page("Login failed", &format!("Google returned an error: {}", err)),
            "400 Bad Request".into(),
        );
    }
    if code.is_empty() {
        return (
            html_page("Login failed", "Missing authorization code in the callback."),
            "400 Bad Request".into(),
        );
    }
    match rt().block_on(xrouter_auth::complete_google_login(&pending, &code)) {
        Ok(acct) => {
            let mut store = xrouter_auth::DeviceStore::load()
                .unwrap_or_else(|_| xrouter_auth::DeviceStore::empty());
            store.add_account(acct.clone());
            if let Err(e) = store.save() {
                return (
                    html_page("Login failed", &format!("Failed to save account: {}", e)),
                    "500 Internal Server Error".into(),
                );
            }
            save_device_account_to_config("antigravity", &acct);
            (
                html_page(
                    "Login complete",
                    &format!(
                        "Connected as <strong>{}</strong>.<br>You can close this tab and return to the wizard.",
                        esc_html(&acct.account_id)
                    ),
                ),
                "200 OK".into(),
            )
        }
        Err(e) => (
            html_page("Login failed", &format!("Token exchange failed: {}", e)),
            "502 Bad Gateway".into(),
        ),
    }
}

/// Build a minimal, self-contained HTML page for the OAuth callback.
fn html_page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title>\
         <style>body{{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;\
         background:#0f1117;color:#e6e6e6;display:flex;min-height:100vh;\
         align-items:center;justify-content:center;margin:0}}div{{max-width:420px;\
         text-align:center;padding:32px;border:1px solid #2a2f3a;border-radius:14px;\
         background:#161a22}}h1{{font-size:1.3rem;margin:0 0 12px}}p{{color:#aab;\
         line-height:1.5;margin:0}}</style></head>\
         <body><div><h1>{}</h1><p>{}</p></div></body></html>",
        esc_html(title),
        esc_html(title),
        body
    )
}

/// Minimal HTML-escape for text injected into the callback page.
fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Record the (non-secret) device account metadata in the main config so the
/// provider shows up as configured. Secrets live only in the device store.
fn save_device_account_to_config(provider: &str, acct: &xrouter_auth::DeviceAccountConfig) {
    let mut cfg = load_config();
    let pcfg = cfg.providers.entry(provider.to_string()).or_insert_with(|| {
        ProviderConfig {
            kind: provider.to_string(),
            base_url: device_base_url(provider),
            enabled: true,
            keys: vec![],
            quota_ban_secs: 300,
            accounts: vec![],
        }
    });
    let exists = pcfg.accounts.iter().any(|a| {
        a.get("account_id").and_then(|v| v.as_str()) == Some(&acct.account_id)
    });
    if !exists {
        pcfg.accounts.push(serde_json::json!({
            "account_id": acct.account_id,
            "display_name": acct.display_name,
            "provider": acct.provider,
            "access_token": "",
            "refresh_token": null,
            "expires_at": null,
        }));
        let _ = save_config(&cfg);
    }
}

/// GET /api/device/accounts — list stored device accounts (metadata only).
fn device_accounts() -> (String, String) {
    let store = match xrouter_auth::DeviceStore::load() {
        Ok(s) => s,
        Err(_) => {
            return (
                serde_json::json!({ "accounts": [] }).to_string(),
                "200 OK".into(),
            )
        }
    };
    let accounts: Vec<Value> = store
        .accounts
        .iter()
        .map(|a| {
            serde_json::json!({
                "provider": a.provider,
                "account_id": a.account_id,
                "display_name": a.display_name,
                "expires_at": a.expires_at.map(|t| t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)),
            })
        })
        .collect();
    (
        serde_json::json!({ "accounts": accounts }).to_string(),
        "200 OK".into(),
    )
}

// ── Router access (API key) ───────────────────────────────────────────────────
//
// These operate on the SAME config.toml the rest of the wizard manages
// (atomic 0600 write via `save_config`). They let the web UI enable/disable the
// router's own Bearer-token auth without dropping the rest of the config.

/// GET /api/auth/status -> { "enabled": bool }
fn auth_status() -> (String, String) {
    let cfg = load_config();
    let enabled = cfg
        .settings
        .api_token
        .as_ref()
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    (
        serde_json::json!({ "enabled": enabled }).to_string(),
        "200 OK".into(),
    )
}

/// POST /api/auth/enable -> generates a key, persists it, returns { "token" }
fn auth_enable() -> (String, String) {
    let mut cfg = load_config();
    let token = gen_router_key();
    cfg.settings.api_token = Some(token.clone());
    match save_config(&cfg) {
        Ok(()) => (
            serde_json::json!({ "token": token }).to_string(),
            "200 OK".into(),
        ),
        Err(e) => (
            json_error(&format!("failed to save config: {}", e)),
            "500 Internal Server Error".into(),
        ),
    }
}

/// POST /api/auth/disable -> removes the key, returns { "ok": true }
fn auth_disable() -> (String, String) {
    let mut cfg = load_config();
    cfg.settings.api_token = None;
    match save_config(&cfg) {
        Ok(()) => (
            serde_json::json!({ "ok": true }).to_string(),
            "200 OK".into(),
        ),
        Err(e) => (
            json_error(&format!("failed to save config: {}", e)),
            "500 Internal Server Error".into(),
        ),
    }
}

// ── HTTP server ──────────────────────────────────────────────────────────────

fn main() {
    let port = std::env::args()
        .position(|a| a == "--port")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(3001);

    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).expect("failed to bind wizard server");
    eprintln!("xrouter-wizard-server listening on http://{}", addr);

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                thread::spawn(move || {
                    if let Err(e) = handle(&mut stream) {
                        eprintln!("wizard request error: {}", e);
                    }
                });
            }
            Err(e) => eprintln!("wizard accept error: {}", e),
        }
    }
}

fn handle(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let mut buf = vec![0u8; 2 * 1024 * 1024];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]).to_string();

    let first = match req.lines().next() {
        Some(l) => l.to_string(),
        None => return Ok(()),
    };
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let full_path = parts.next().unwrap_or("/").to_string();

    let (path, query) = match full_path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (full_path, String::new()),
    };

    // Body (everything after the header terminator).
    let body = if let Some(pos) = req.find("\r\n\r\n") {
        req[pos + 4..].to_string()
    } else if let Some(pos) = req.find("\n\n") {
        req[pos + 2..].to_string()
    } else {
        String::new()
    };

    match (method.as_str(), path.as_str()) {
        ("GET", "/") => respond(stream, xrouter_wizard::WIZARD_HTML, "200 OK", "text/html; charset=utf-8"),
        ("GET", "/metrics") => {
            respond(stream, xrouter_wizard::METRICS_HTML, "200 OK", "text/html; charset=utf-8")
        }
        ("GET", "/api/config") => {
            let cfg = load_config();
            let json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".into());
            respond(stream, &json, "200 OK", "application/json")
        }
        ("POST", "/api/config") => {
            let (json, status) = post_config(&body, &config_path());
            respond(stream, &json, &status, "application/json")
        }
        ("GET", "/api/models/fetch") => {
            let provider = query
                .split('&')
                .find_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    if k == "provider" {
                        Some(url_decode(v))
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let (json, status) = fetch_models(&provider);
            respond(stream, &json, &status, "application/json")
        }
        ("GET", "/api/device/accounts") => {
            let (json, status) = device_accounts();
            respond(stream, &json, &status, "application/json")
        }
        ("GET", "/oauth/callback") => {
            let (html, status) = device_oauth_callback(&query);
            respond(stream, &html, &status, "text/html; charset=utf-8")
        }
        ("POST", "/api/device/login") => {
            let (json, status) = device_login(&body);
            respond(stream, &json, &status, "application/json")
        }
        ("POST", "/api/device/poll") => {
            let (json, status) = device_poll(&body);
            respond(stream, &json, &status, "application/json")
        }
        ("GET", "/api/auth/status") => {
            let (json, status) = auth_status();
            respond(stream, &json, &status, "application/json")
        }
        ("POST", "/api/auth/enable") => {
            let (json, status) = auth_enable();
            respond(stream, &json, &status, "application/json")
        }
        ("POST", "/api/auth/disable") => {
            let (json, status) = auth_disable();
            respond(stream, &json, &status, "application/json")
        }
        _ => respond(stream, "<h1>404 Not Found</h1>", "404 Not Found", "text/html"),
    }
}

fn respond(
    stream: &mut std::net::TcpStream,
    body: &str,
    status: &str,
    content_type: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n{}",
        status,
        content_type,
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod persist_integration_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique temp config path (does NOT touch XROUTER_CONFIG so tests stay
    /// parallel-safe).
    fn tmp_config_path() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("xr_wiz_persist_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn post_new_tier_persists_and_coexists_with_builtins() {
        let p = tmp_config_path();
        let _ = std::fs::remove_file(&p);

        // Simulate the exact JSON body the wizard frontend sends for a new
        // custom tier (POST /api/config handler deserializes this into Config).
        let body = serde_json::json!({
            "settings": {"default_tier": null},
            "providers": {
                "opencode-zen": {"kind":"openai-compat","base_url":"https://opencode.ai/zen/v1","enabled":true,"keys":[],"quota_ban_secs":300}
            },
            "tiers": [{
                "name":"user-tier","strict":true,"default_entry":0,
                "entries":[{"provider":"opencode-zen","model":"big-pickle","is_default":true,"weight":1,"endpoint_id":""}]
            }]
        })
        .to_string();

        let cfg: Config = serde_json::from_str(&body).expect("deserialize posted config");
        assert!(validate_config(&cfg).is_none(), "valid config must pass validation");
        save_config_to(&cfg, &p).expect("save must succeed");

        // GET equivalent: load_config merges builtins into whatever was saved.
        let loaded = load_config_from(&p);
        let names: Vec<&str> = loaded.tiers.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"user-tier"), "custom tier must persist: {:?}", names);
        assert!(names.contains(&"big-pickle"), "builtin big-pickle must coexist");
        assert!(names.contains(&"images"), "builtin images must coexist");

        // File on disk must contain the custom tier.
        let on_disk = std::fs::read_to_string(&p).expect("read saved file");
        assert!(on_disk.contains("user-tier"), "saved file must contain custom tier");
        assert!(on_disk.contains("big-pickle"), "saved file must contain builtin tier");

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn invalid_tier_name_is_rejected_by_validation() {
        let body = serde_json::json!({
            "settings": {"default_tier": null},
            "providers": {},
            "tiers": [{"name":"bad name!", "strict":true,"default_entry":0,"entries":[]}]
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&body).unwrap();
        let err = validate_config(&cfg);
        assert!(err.is_some(), "invalid tier name must be rejected");
        assert!(err.unwrap().contains("invalid tier name"), "error should mention tier name");
    }

    #[test]
    fn empty_provider_or_model_entry_is_rejected() {
        let body = serde_json::json!({
            "settings": {"default_tier": null},
            "providers": {},
            "tiers": [{"name":"t","strict":true,"default_entry":0,"entries":[{"provider":"","model":"x","is_default":true,"weight":1,"endpoint_id":""}]}]
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&body).unwrap();
        assert!(validate_config(&cfg).is_some(), "empty provider entry must be rejected");
    }

    #[test]
    fn post_config_with_key_verifies_and_persists() {
        let p = tmp_config_path();
        let _ = std::fs::remove_file(&p);

        let body = serde_json::json!({
            "settings": {"default_tier": null, "api_token": null},
            "providers": {
                "opencode-zen": {
                    "kind": "openai-compat",
                    "base_url": "https://opencode.ai/zen/v1",
                    "enabled": true,
                    "keys": ["sk-secret-123"],
                    "quota_ban_secs": 300
                }
            },
            "tiers": [{
                "name": "big-pickle", "strict": true, "default_entry": 0,
                "entries": [{"provider":"opencode-zen","model":"big-pickle","is_default":true,"weight":1,"endpoint_id":""}]
            }]
        })
        .to_string();

        let (json, status) = post_config(&body, &p);
        assert_eq!(status, "200 OK", "expected 200, got {}", status);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true, "ok must be true: {}", json);
        assert_eq!(v["verified"], true, "verified must be true: {}", json);
        assert_eq!(v["providers"]["opencode-zen"]["keys"], 1, "key count must be 1: {}", json);
        assert_eq!(v["providers"]["opencode-zen"]["enabled"], true);
        assert!(v["tiers"].as_array().unwrap().contains(&serde_json::json!("big-pickle")));

        // The file on disk must actually contain the key (the bug we guard against).
        let on_disk = std::fs::read_to_string(&p).expect("read saved file");
        assert!(on_disk.contains("sk-secret-123"), "key must be persisted to disk: {}", on_disk);

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn post_config_write_failure_reports_unverified() {
        let dir = std::env::temp_dir().join(format!(
            "xr_notdir_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        // Make `dir` a regular FILE so that the config path `dir/config.toml`
        // has a parent that cannot be a directory. The atomic write then fails
        // reliably — this works even when running as root, which bypasses
        // read-only *directory* permissions.
        std::fs::write(&dir, b"not a directory").unwrap();
        let p = dir.join("config.toml");

        let body = serde_json::json!({
            "settings": {"default_tier": null},
            "providers": {
                "opencode-zen": {
                    "kind": "openai-compat",
                    "base_url": "https://opencode.ai/zen/v1",
                    "enabled": true,
                    "keys": ["k"],
                    "quota_ban_secs": 300
                }
            },
            "tiers": []
        })
        .to_string();

        let (json, status) = post_config(&body, &p);
        assert_eq!(
            status, "500 Internal Server Error",
            "write failure must yield 500, got {} ({})",
            status, json
        );
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], false, "ok must be false on write failure: {}", json);
        let err = v["error"].as_str().expect("error string").to_string();
        assert!(
            err.contains("save verification failed"),
            "error must mention save verification failed: {}",
            err
        );

        let _ = std::fs::remove_file(&dir);
    }
}
