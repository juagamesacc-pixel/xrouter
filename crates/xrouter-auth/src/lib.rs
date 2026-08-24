//! xrouter-auth — off-RAM device-login token store and helpers.
//!
//! This crate owns everything related to the OAuth2 *device authorization
//! grant* flow used by device-kind providers (`kiro`, `antigravity`, and the
//! generic `device:<name>` form). Secrets (access/refresh tokens) live here in
//! a separate, 0600-on-disk store so they never enter the main `Config`
//! (which only carries non-secret account metadata).
//!
//! The crate is intentionally tiny and dependency-light: it is linked into the
//! server, balancer, config and CLI crates but performs no work unless a
//! device-kind provider is actually configured (see `is_device_kind`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

/// Provider names that authenticate via the device-login flow.
pub const DEVICE_PROVIDERS: &[&str] = &["kiro", "antigravity"];

/// True if `kind` is a device-login provider (`kiro`, `antigravity`,
/// `device:<name>`).
pub fn is_device_kind(kind: &str) -> bool {
    kind == "kiro" || kind == "antigravity" || kind.starts_with("device:")
}

/// Bare provider name for a device `kind` (`device:kiro` -> `kiro`,
/// `kiro` -> `kiro`). Returns a slice of the input, so the lifetime is tied to
/// `kind`.
pub fn normalize_provider(kind: &str) -> &str {
    if let Some(rest) = kind.strip_prefix("device:") {
        rest
    } else {
        kind
    }
}

/// Extra static headers required on upstream requests for a device provider.
/// Most device providers only need the bearer access token (handled by the
/// adapter); `antigravity` additionally requires a static client header.
pub fn upstream_extra_headers(kind: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    if normalize_provider(kind) == "antigravity" {
        h.insert(
            HeaderName::from_static("x-cline-client"),
            HeaderValue::from_static("xrouter"),
        );
    }
    h
}

/// Non-secret account metadata + (secret) tokens for a single device login.
///
/// Serialized into the off-RAM device store. `account_id` + `provider` form the
/// unique key used by [`DeviceStore::add_account`] for de-duplication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceAccountConfig {
    /// Stable per-provider identifier chosen at login time (e.g. email/local id).
    pub account_id: String,
    /// Human-friendly label (may be the same as `account_id`).
    pub display_name: Option<String>,
    /// Bare provider name (`kiro`, `antigravity`, ...).
    pub provider: String,
    /// Current OAuth access token, sent upstream as `Bearer`.
    pub access_token: String,
    /// Refresh token (if the provider issued one). `None` means we can only use
    /// the access token until it expires.
    pub refresh_token: Option<String>,
    /// Absolute expiry of `access_token`. `None` means "unknown / never".
    pub expires_at: Option<SystemTime>,
}

impl DeviceAccountConfig {
    /// True when the access token is expired (or will expire within 60s).
    fn needs_refresh(&self) -> bool {
        match self.expires_at {
            Some(exp) => SystemTime::now() + Duration::from_secs(60) >= exp,
            None => false,
        }
    }
}

/// Off-RAM, on-disk store of device accounts. Secrets never enter `Config`.
///
/// The `rr` field tracks round-robin position per provider so that
/// [`DeviceStore::next_account`] cycles through accounts without duplicating
/// them. It is skipped during (de)serialization.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceStore {
    pub accounts: Vec<DeviceAccountConfig>,
    #[serde(skip)]
    rr: HashMap<String, usize>,
}

impl DeviceStore {
    /// An empty store (no accounts, no disk access).
    pub fn empty() -> Self {
        Self {
            accounts: Vec::new(),
            rr: HashMap::new(),
        }
    }

    /// Load the store from disk. Returns an empty store when the file does not
    /// exist yet (first run). Errors only on malformed/unreadable content.
    pub fn load() -> Result<Self> {
        let p = store_path();
        if !p.exists() {
            return Ok(Self::empty());
        }
        let s = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let store: DeviceStore = serde_json::from_str(&s).context("parse device store")?;
        Ok(store)
    }

    /// Persist the store to disk with 0600 permissions (secrets!).
    pub fn save(&self) -> Result<()> {
        let p = store_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).context("create device store dir")?;
        }
        let json = serde_json::to_string_pretty(self).context("serialize device store")?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, &json).context("write tmp")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &p).context("rename device store")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Insert or replace an account, keyed by `(provider, account_id)`.
    pub fn add_account(&mut self, acct: DeviceAccountConfig) {
        if let Some(pos) = self
            .accounts
            .iter()
            .position(|a| a.provider == acct.provider && a.account_id == acct.account_id)
        {
            self.accounts[pos] = acct;
        } else {
            self.accounts.push(acct);
        }
    }

    /// Remove a single account by `(provider, account_id)`.
    pub fn remove_account(&mut self, provider: &str, account: &str) {
        self.accounts
            .retain(|a| !(a.provider == provider && a.account_id == account));
    }

    /// Number of accounts configured for `provider`.
    pub fn count_for(&self, provider: &str) -> usize {
        self.accounts
            .iter()
            .filter(|a| a.provider == provider)
            .count()
    }

    /// Return the next account for `provider` in round-robin order. Returns a
    /// *clone* so the store retains the account (the caller re-adds only on
    /// refresh/401 paths; on success the stored copy stays valid).
    pub fn next_account(&mut self, provider: &str) -> Option<DeviceAccountConfig> {
        let idxs: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.provider == provider)
            .map(|(i, _)| i)
            .collect();
        if idxs.is_empty() {
            return None;
        }
        let next = self.rr.get(provider).copied().unwrap_or(0);
        let pick = idxs[next % idxs.len()];
        self.rr
            .insert(provider.to_string(), (next + 1) % idxs.len());
        Some(self.accounts[pick].clone())
    }
}

/// Refresh `acct`'s access token if it is expired (or about to expire).
///
/// No-op when the token is still valid or when there is no `refresh_token`.
/// Network failures are returned as errors; callers typically only warn.
pub async fn refresh_if_needed(acct: &mut DeviceAccountConfig) -> Result<()> {
    if !acct.needs_refresh() {
        return Ok(());
    }
    let refresh_token = match &acct.refresh_token {
        Some(t) => t.clone(),
        None => return Ok(()),
    };
    let token_url = token_endpoint(&acct.provider);
    let client = reqwest::Client::new();
    let resp = client
        .post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
        ])
        .send()
        .await
        .context("device token refresh request")?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("device token refresh failed ({}): {}", status, txt);
    }
    let v: serde_json::Value = resp.json().await.context("parse token refresh response")?;
    if let Some(tok) = v.get("access_token").and_then(|x| x.as_str()) {
        acct.access_token = tok.to_string();
    }
    if let Some(rt) = v.get("refresh_token").and_then(|x| x.as_str()) {
        acct.refresh_token = Some(rt.to_string());
    }
    if let Some(secs) = v.get("expires_in").and_then(|x| x.as_u64()) {
        acct.expires_at = Some(SystemTime::now() + Duration::from_secs(secs));
    }
    Ok(())
}

/// In-progress device login, returned by [`initiate_device_login`] and polled
/// by [`poll_device_login`].
#[derive(Debug, Clone)]
pub struct DeviceLoginInit {
    pub provider: String,
    pub account: String,
    pub display: String,
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
}

/// Begin a device authorization flow. Returns the verification URI + user code
/// the human must visit, plus the `device_code` used while polling.
pub async fn initiate_device_login(
    provider: &str,
    account: &str,
    display: &str,
) -> Result<DeviceLoginInit> {
    let url = device_auth_endpoint(provider);
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .form(&[
            ("client_id", client_id(provider)),
            ("scope", scope_for(provider).to_string()),
            ("account", account.to_string()),
        ])
        .send()
        .await
        .context("device auth request")?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("device auth init failed ({}): {}", status, txt);
    }
    let v: serde_json::Value = resp.json().await.context("parse device auth response")?;
    let init = DeviceLoginInit {
        provider: provider.to_string(),
        account: account.to_string(),
        display: display.to_string(),
        device_code: v
            .get("device_code")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        user_code: v
            .get("user_code")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        verification_uri: v
            .get("verification_uri")
            .or_else(|| v.get("verification_url"))
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        interval: v.get("interval").and_then(|x| x.as_u64()).unwrap_or(5),
    };
    Ok(init)
}

/// Poll the token endpoint until the user completes the device login or the
/// flow errors out. Returns the populated [`DeviceAccountConfig`].
pub async fn poll_device_login(init: &DeviceLoginInit) -> Result<DeviceAccountConfig> {
    let url = token_endpoint(&init.provider);
    let client = reqwest::Client::new();
    let deadline = SystemTime::now() + Duration::from_secs(600);
    loop {
        if SystemTime::now() > deadline {
            anyhow::bail!("device login timed out");
        }
        let resp = client
            .post(&url)
            .form(&[
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code",
                ),
                ("device_code", init.device_code.as_str()),
                ("client_id", client_id(&init.provider).as_str()),
            ])
            .send()
            .await
            .context("device token poll")?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.context("parse token poll response")?;
        if status.is_success() {
            let access_token = v
                .get("access_token")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let refresh_token = v
                .get("refresh_token")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let expires_at = v
                .get("expires_in")
                .and_then(|x| x.as_u64())
                .map(|s| SystemTime::now() + Duration::from_secs(s));
            return Ok(DeviceAccountConfig {
                account_id: init.account.clone(),
                display_name: Some(init.display.clone()),
                provider: init.provider.clone(),
                access_token,
                refresh_token,
                expires_at,
            });
        }
        let err = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
        if err == "authorization_pending" {
            tokio::time::sleep(Duration::from_secs(init.interval.max(1))).await;
            continue;
        } else if err == "slow_down" {
            tokio::time::sleep(Duration::from_secs((init.interval * 2).max(1))).await;
            continue;
        }
        anyhow::bail!("device login failed: {}", err);
    }
}

// --- provider-specific endpoints (placeholders; real hosts filled by config) --

fn store_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/xrouter/device-store.json")
}

fn device_auth_endpoint(provider: &str) -> String {
    match provider {
        "kiro" => "https://api.kiro.dev/oauth/device".to_string(),
        "antigravity" => "https://api.cline.bot/oauth/device".to_string(),
        other => format!("https://{}.invalid/oauth/device", other),
    }
}

fn token_endpoint(provider: &str) -> String {
    match provider {
        "kiro" => "https://api.kiro.dev/oauth/token".to_string(),
        "antigravity" => "https://api.cline.bot/oauth/token".to_string(),
        other => format!("https://{}.invalid/oauth/token", other),
    }
}

fn client_id(provider: &str) -> String {
    match provider {
        "kiro" => "xrouter-kiro".to_string(),
        "antigravity" => "xrouter-antigravity".to_string(),
        other => format!("xrouter-{}", other),
    }
}

fn scope_for(provider: &str) -> &'static str {
    match provider {
        "kiro" => "openid profile",
        "antigravity" => "openid profile",
        _ => "openid",
    }
}

/// Helper: current unix-epoch seconds (used by callers that log expiry).
#[allow(dead_code)]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
