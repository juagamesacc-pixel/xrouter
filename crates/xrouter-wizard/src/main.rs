/// Minimal HTTP server that serves the self-contained wizard HTML templates
/// produced by the `xrouter-wizard` library crate, plus a small JSON API that
/// proxies model discovery and persists the wizard's configuration.
///
/// The `xrouter` CLI spawns this binary (as `xrouter-wizard-server`) on :3001
/// for the `xrouter wizard --web` flow. It deliberately depends only on
/// `std` + `serde_json`/`toml` so it stays out of the main server hot path.
///
/// Routes:
///   GET /                     -> WIZARD_HTML
///   GET /metrics              -> METRICS_HTML
///   GET /api/config           -> current config as JSON
///   POST /api/config          -> persist config (JSON body) to config.toml
///   GET /api/models/fetch?provider=X
///                             -> server-side proxy to the provider's /models
///   anything else             -> 404

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

fn load_config() -> Config {
    let path = config_path();
    let mut cfg: Config = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    };
    for (id, p) in builtin_providers() {
        cfg.providers.entry(id).or_insert(p);
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

/// Proxies `GET {base_url}/models` for the given provider using its first key.
/// Returns (json_body, http_status). On any failure the body is
/// `{ "error": "..." }` so the frontend can surface a custom modal.
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
            json_error(&format!(
                "no API key configured for provider '{}'",
                provider
            )),
            "400 Bad Request".into(),
        );
    }
    let base = p.base_url.trim_end_matches('/');
    let url = format!("{}/models", base);
    let key = &p.keys[0];

    let out = Command::new("curl")
        .args([
            "-s",
            "-m",
            "25",
            "-w",
            "\n%{http_code}",
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

    let stdout = String::from_utf8_lossy(&out.stdout);
    let s = stdout.trim_end().to_string();
    let (body, code) = match s.rfind('\n') {
        Some(idx) => (s[..idx].to_string(), s[idx + 1..].trim().to_string()),
        None => (String::new(), s.trim().to_string()),
    };

    if !code.starts_with('2') {
        return (
            json_error(&format!("provider returned HTTP {}: {}", code, body.trim())),
            "502 Bad Gateway".into(),
        );
    }

    match serde_json::from_str::<Value>(&body) {
        Ok(v) => {
            let data = v
                .get("data")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default();
            let ids: Vec<Value> = data
                .iter()
                .filter_map(|m| {
                    let id = m
                        .get("id")
                        .and_then(|i| i.as_str())
                        .or_else(|| m.get("name").and_then(|n| n.as_str()));
                    id.map(|id| serde_json::json!({ "id": id }))
                })
                .collect();
            (
                serde_json::json!({ "data": ids }).to_string(),
                "200 OK".into(),
            )
        }
        Err(e) => (
            json_error(&format!("invalid JSON from provider: {}", e)),
            "502 Bad Gateway".into(),
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
        ("POST", "/api/config") => match serde_json::from_str::<Config>(&body) {
            Ok(cfg) => match save_config(&cfg) {
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
            },
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
