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
///   POST /api/device/login    -> begin device login, returns code + uri
///   POST /api/device/poll     -> poll until approved, then persist account
///   anything else             -> 404

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::OnceLock;
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use xrouter_auth;

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

fn load_config() -> Config {
    let path = config_path();
    let mut cfg: Config = match std::fs::read_to_string(&path) {
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

fn save_config(cfg: &Config) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let toml = toml::to_string_pretty(cfg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, toml)
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
    let key = &p.keys[0];

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
            return (
                json_error(&format!("failed to invoke curl: {}", e)),
                "500 Internal Server Error".into(),
            )
        }
    };

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if stderr.is_empty() {
            format!("curl exited with status {}", out.status)
        } else {
            stderr
        };
        return (json_error(&msg), "502 Bad Gateway".into());
    }

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    match serde_json::from_str::<Value>(&stdout) {
        Ok(v) => {
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
            (
                serde_json::json!({ "data": models }).to_string(),
                "200 OK".into(),
            )
        }
        Err(e) => (
            json_error(&format!("invalid JSON from provider: {}", e)),
            "502 Bad Gateway".into(),
        ),
    }
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
/// Begins a device authorization flow and returns the verification URI + user
/// code (plus the device_code the UI must send to /api/device/poll). The
/// account identity is derived automatically after the login completes — no
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
    // No manual account id — identity is derived from the signed-in provider
    // account after the device login completes.
    match rt().block_on(xrouter_auth::initiate_device_login(
        &prov_name,
        "",
        "",
    )) {
        Ok(init) => {
            let resp = serde_json::json!({
                "verification_uri": init.verification_uri,
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

/// POST /api/device/poll {provider, device_code, poll_token?}
/// Polls (blocking, up to the provider's timeout) until the user approves the
/// device login, then persists the account to the off-RAM device store and to
/// the config (metadata only, no secrets).
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
    let init = xrouter_auth::DeviceLoginInit {
        provider: prov_name.clone(),
        account: String::new(),
        display: String::new(),
        device_code: device_code.clone(),
        user_code: String::new(),
        verification_uri: String::new(),
        interval: 5,
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
        ("POST", "/api/config") => match serde_json::from_str::<Config>(&body) {
            Ok(cfg) => {
                if let Some(err) = validate_config(&cfg) {
                    respond(
                        stream,
                        &json_error(&err),
                        "400 Bad Request",
                        "application/json",
                    )
                } else {
                    match save_config(&cfg) {
                        Ok(()) => respond(
                            stream,
                            &serde_json::json!({ "ok": true }).to_string(),
                            "200 OK",
                            "application/json",
                        ),
                        Err(e) => respond(
                            stream,
                            &json_error(&format!("failed to save config: {}", e)),
                            "500 Internal Server Error",
                            "application/json",
                        ),
                    }
                }
            }
            Err(e) => respond(
                stream,
                &json_error(&format!("invalid config json: {}", e)),
                "400 Bad Request",
                "application/json",
            ),
        },
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
        ("POST", "/api/device/login") => {
            let (json, status) = device_login(&body);
            respond(stream, &json, &status, "application/json")
        }
        ("POST", "/api/device/poll") => {
            let (json, status) = device_poll(&body);
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
