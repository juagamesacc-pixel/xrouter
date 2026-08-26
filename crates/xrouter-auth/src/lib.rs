//! xrouter-auth — off-RAM device-login token store and helpers.
//!
//! This crate owns everything related to the OAuth2 login flows used by
//! device-kind providers (`kiro`, `antigravity`, and the generic `device:<name>`
//! form). Secrets (access/refresh tokens) live here in a separate, 0600-on-disk
//! store so they never enter the main `Config` (which only carries non-secret
//! account metadata).
//!
//! The crate is intentionally tiny and dependency-light: it is linked into the
//! server, balancer, config and CLI crates but performs no work unless a
//! device-kind provider is actually configured (see `is_device_kind`).
//!
//! ## Login flows
//!
//! * `kiro` (AWS Builder ID) — standard OAuth2 **device authorization** grant
//!   against the AWS SSO OIDC endpoints (`oidc.<region>.amazonaws.com`). The
//!   flow is: dynamic client registration → device authorization → poll token.
//! * `antigravity` (Google) — **PKCE authorization-code** grant with a loopback
//!   redirect. Google does *not* support the device-code grant for these
//!   clients, so we build a consent URL (with `code_challenge_method=S256`),
//!   the caller opens it in a browser, and a local HTTP listener receives the
//!   `?code` callback which we exchange for tokens.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rand::RngExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use url::Url;

/// SECURITY NOTE — secret handling.
///
/// All device-login secrets (OAuth access/refresh tokens) are exchanged and
/// refreshed here via `reqwest` **only**. Never shell out to `curl` (or any
/// external process) to perform these exchanges: command-line arguments and
/// environment are visible to other local users via `ps`/`/proc`, which would
/// leak the tokens. Other crates in this workspace (e.g. the wizard) should
/// likewise prefer `reqwest` over spawning `curl` for any secret-bearing
/// request.

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
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceAccountConfig {
    /// Stable per-provider identifier chosen at login time (e.g. email/local id).
    pub account_id: String,
    /// Human-friendly label (may be the same as `account_id`).
    pub display_name: Option<String>,
    /// Bare provider name (`kiro`, `antigravity`, ...).
    pub provider: String,
    /// Current OAuth access token, sent upstream as `Bearer`.
    ///
    /// This is a secret. It is intentionally **not** shown by the `Debug` impl
    /// (see the manual `impl Debug` below) and should never be logged in full.
    pub access_token: String,
    /// Refresh token (if the provider issued one). `None` means we can only use
    /// the access token until it expires.
    pub refresh_token: Option<String>,
    /// Absolute expiry of `access_token`. `None` means "unknown / never".
    pub expires_at: Option<SystemTime>,
    /// OAuth client id issued at login (kiro: from dynamic registration;
    /// antigravity: the public Google client id). Needed for token refresh.
    #[serde(default)]
    pub client_id: Option<String>,
    /// OAuth client secret (kiro: from dynamic registration; antigravity: the
    /// public Google client secret). Needed for token refresh.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Q Developer / CodeWhisperer profile ARN discovered after a successful
    /// device-flow token exchange or refresh (kiro/Builder ID only). `None`
    /// when discovery was skipped or unavailable. Persisted to the off-RAM
    /// store but never used for authorization.
    #[serde(default)]
    pub profile_arn: Option<String>,
}

impl DeviceAccountConfig {
    /// Masked view of the access token for logs/display.
    ///
    /// JWTs must **never** be truncated to their last 4 characters (that leaks
    /// a meaningful fraction of the credential). Instead we either show a
    /// constant placeholder, or — for tokens that look like JWTs (`eyJ…`) — a
    /// 3-character `eyJ…` prefix that only identifies the token type without
    /// disclosing any secret material.
    pub fn masked_access_token(&self) -> String {
        if self.access_token.starts_with("eyJ") {
            format!("{}…", &self.access_token[..self.access_token.len().min(3)])
        } else {
            "<device-token>".to_string()
        }
    }
}

impl std::fmt::Debug for DeviceAccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAccountConfig")
            .field("account_id", &self.account_id)
            .field("display_name", &self.display_name)
            .field("provider", &self.provider)
            .field("access_token", &"<device-token>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<device-token>"),
            )
            .field("expires_at", &self.expires_at)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("profile_arn", &self.profile_arn)
            .finish()
    }
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
/// them. It is skipped during (de)serialization via `#[serde(skip)]`.
///
/// NOTE: because `rr` is `#[serde(skip)]`, the round-robin cursor is **reset to
/// zero on every process restart** (it is not persisted to disk). This is
/// acceptable: it only affects the starting point of the cycle, not correctness,
/// and avoids leaking provider-account selection order into the on-disk secret
/// store. If strict stickiness across restarts is ever required, switch this to
/// `#[serde(default)]` (a `HashMap` deserializes fine from an absent field) so
/// the cursor is persisted alongside the accounts.
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
        let p = store_path()?;
        if !p.exists() {
            return Ok(Self::empty());
        }
        let s = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let store: DeviceStore = serde_json::from_str(&s).context("parse device store")?;
        Ok(store)
    }

    /// Persist the store to disk with 0600 permissions (secrets!).
    pub fn save(&self) -> Result<()> {
        let p = store_path()?;
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

    /// Remove every account for `provider` (used by `device logout` without an
    /// explicit identity — drops all signed-in accounts for that provider).
    pub fn remove_accounts_for_provider(&mut self, provider: &str) {
        self.accounts.retain(|a| a.provider != provider);
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
///
/// On a kiro/Builder ID refresh that fails with a grant/expiry error
/// (`InvalidGrantException`, `ExpiredTokenException`, or `invalid_grant`) the
/// client registration may have expired, so we re-register the client once and
/// retry the refresh (mirrors OmniRoute). If the retry still fails, the error
/// is returned.
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

    let v = match do_token_refresh(&client, &token_url, acct, &refresh_token).await {
        Ok(v) => v,
        Err(e) => {
            let msg = e.to_string();
            if is_grant_expired_error(&msg) && normalize_provider(&acct.provider) == "kiro" {
                tracing::warn!(
                    "kiro token refresh failed with grant/expiry error; re-registering client and retrying once: {msg}"
                );
                let region = kiro_region();
                let (cid, csec) = register_kiro_client(&region).await?;
                acct.client_id = Some(cid);
                acct.client_secret = Some(csec);
                do_token_refresh(&client, &token_url, acct, &refresh_token).await?
            } else {
                return Err(e);
            }
        }
    };

    let old_token = acct.access_token.clone();
    if let Some(tok) = v.get("access_token").and_then(|x| x.as_str()) {
        acct.access_token = tok.to_string();
    }
    if let Some(rt) = v.get("refresh_token").and_then(|x| x.as_str()) {
        acct.refresh_token = Some(rt.to_string());
    }
    if let Some(secs) = v.get("expires_in").and_then(|x| x.as_u64()) {
        acct.expires_at = Some(SystemTime::now() + Duration::from_secs(secs));
    }
    // If the access token actually changed, the signed-in identity may have
    // changed too (e.g. a rotated JWT). Re-derive and update the account key so
    // the off-RAM store stays consistent.
    if acct.access_token != old_token {
        let identity = derive_identity(&acct.provider, &acct.access_token).await;
        if identity != acct.account_id {
            acct.account_id = identity.clone();
            acct.display_name = Some(identity);
        }
    }
    // Best-effort profile ARN discovery (kiro only) + Builder ID token prefix
    // sanity check. Neither is fatal.
    if let Some(arn) = discover_kiro_profile_arn(acct).await {
        acct.profile_arn = Some(arn);
    }
    check_builder_id_token_prefix(acct);
    Ok(())
}

/// Perform a single OAuth2 token-refresh request and return the parsed JSON
/// response. Extracted from [`refresh_if_needed`] so the grant-expiry retry
/// path can reuse it without duplicating the form construction.
async fn do_token_refresh(
    client: &reqwest::Client,
    token_url: &str,
    acct: &DeviceAccountConfig,
    refresh_token: &str,
) -> Result<serde_json::Value> {
    // AWS SSO OIDC CreateToken expects a JSON body with camelCase keys
    // (mirrors OmniRoute's refresh); form-encoded snake_case yields 400
    // invalid_request once codewhisperer scopes are on the registration.
    let mut body = serde_json::json!({
        "grantType": "refresh_token",
        "refreshToken": refresh_token
    });
    if let Some(cid) = &acct.client_id {
        body["clientId"] = serde_json::Value::String(cid.clone());
    }
    if let Some(csec) = &acct.client_secret {
        body["clientSecret"] = serde_json::Value::String(csec.clone());
    }
    let resp = client
        .post(token_url)
        .json(&body)
        .send()
        .await
        .context("device token refresh request")?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("device token refresh failed ({}): {}", status, txt);
    }
    resp.json().await.context("parse token refresh response")
}

/// True when a refresh error indicates the client registration or token grant
/// has expired/been revoked and a fresh client registration may help.
fn is_grant_expired_error(msg: &str) -> bool {
    msg.contains("InvalidGrantException")
        || msg.contains("ExpiredTokenException")
        || msg.contains("invalid_grant")
}

// ── Kiro (AWS Builder ID) device-authorization flow ───────────────────────────

/// In-progress kiro device login, returned by [`initiate_device_login`] and
/// polled by [`poll_device_login`].
#[derive(Debug, Clone)]
pub struct DeviceLoginInit {
    pub provider: String,
    pub account: String,
    pub display: String,
    pub device_code: String,
    pub user_code: String,
    /// `verification_uri_complete` (one-click) when available, else the bare
    /// `verification_uri`.
    pub verification_uri: String,
    /// `verification_uri_complete` exactly as returned by AWS (one-click link).
    pub verification_uri_complete: String,
    pub interval: u64,
    /// OAuth client id issued by AWS during dynamic registration.
    pub client_id: String,
    /// OAuth client secret issued by AWS during dynamic registration.
    pub client_secret: String,
}

/// Begin a device authorization flow for `provider`.
///
/// * `kiro` — AWS SSO OIDC device flow (register → device_authorization).
/// * `antigravity` — not a device-code flow; callers must use
///   [`build_google_auth_url`] + [`complete_google_login`] instead. This
///   returns a clear error directing to that flow.
pub async fn initiate_device_login(
    provider: &str,
    account: &str,
    display: &str,
) -> Result<DeviceLoginInit> {
    let prov = normalize_provider(provider);
    match prov {
        "kiro" => initiate_kiro_login(account, display).await,
        "antigravity" => anyhow::bail!(
            "antigravity uses a PKCE browser flow, not device-code; call build_google_auth_url()"
        ),
        other => anyhow::bail!(
            "unsupported device provider '{}' (supported: kiro, antigravity)",
            other
        ),
    }
}

/// AWS SSO OIDC dynamic client registration. Returns `(client_id, client_secret)`.
///
/// The requested scopes are expanded beyond the bare `openid profile` to include
/// the CodeWhisperer/Q Developer scopes (mirrors OmniRoute) and are configurable
/// via the `KIRO_SCOPES` env var (see [`kiro_scopes`]).
async fn register_kiro_client(region: &str) -> Result<(String, String)> {
    assert_valid_aws_region(region)?;
    let url = kiro_register_endpoint(region);
    let client = reqwest::Client::new();
    // AWS SSO OIDC RegisterClient expects a JSON body with `scopes` as an
    // ARRAY, and `grantTypes` is REQUIRED whenever scopes are requested
    // (omitting it yields 400 invalid_request). Mirrors OmniRoute's payload.
    let scopes: Vec<String> = kiro_scopes().split_whitespace().map(str::to_string).collect();
    let body = serde_json::json!({
        "clientName": "xrouter",
        "clientType": "public",
        "scopes": scopes,
        "grantTypes": [
            "urn:ietf:params:oauth:grant-type:device_code",
            "refresh_token"
        ]
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("kiro client register request")?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("kiro client register failed ({}): {}", status, txt);
    }
    let v: serde_json::Value = resp.json().await.context("parse kiro register response")?;
    let client_id = v
        .get("clientId")
        .or_else(|| v.get("client_id"))
        .and_then(|x| x.as_str())
        .context("kiro register response missing clientId")?
        .to_string();
    let client_secret = v
        .get("clientSecret")
        .or_else(|| v.get("client_secret"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    Ok((client_id, client_secret))
}

/// AWS SSO OIDC device authorization. Returns the device code + verification
/// URI the human must visit.
async fn kiro_device_authorization(
    region: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<DeviceLoginInit> {
    let url = kiro_device_auth_endpoint(region);
    let client = reqwest::Client::new();
    // AWS SSO OIDC StartDeviceAuthorization expects a JSON body with
    // clientId/clientSecret/startUrl (scopes were already granted at client
    // registration — sending them here causes 400 invalid_request).
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "startUrl": "https://view.awsapps.com/start"
        }))
        .send()
        .await
        .context("kiro device auth request")?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("kiro device auth failed ({}): {}", status, txt);
    }
    let v: serde_json::Value = resp.json().await.context("parse kiro device auth response")?;
    let device_code = v
        .get("deviceCode")
        .or_else(|| v.get("device_code"))
        .and_then(|x| x.as_str())
        .context("kiro device auth response missing deviceCode")?
        .to_string();
    let user_code = v
        .get("userCode")
        .or_else(|| v.get("user_code"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let verification_uri_complete = v
        .get("verificationUriComplete")
        .or_else(|| v.get("verification_uri_complete"))
        .and_then(|x| x.as_str())
        .or_else(|| {
            v.get("verificationUri")
                .or_else(|| v.get("verification_uri"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or_default()
        .to_string();
    let verification_uri = v
        .get("verificationUri")
        .or_else(|| v.get("verification_uri"))
        .and_then(|x| x.as_str())
        .unwrap_or(&verification_uri_complete)
        .to_string();
    let interval = v
        .get("interval")
        .and_then(|x| x.as_u64())
        .unwrap_or(5);
    Ok(DeviceLoginInit {
        provider: "kiro".to_string(),
        account: String::new(),
        display: String::new(),
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
        interval,
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
    })
}

/// Begin the kiro device login: register a client, then request a device code.
async fn initiate_kiro_login(account: &str, display: &str) -> Result<DeviceLoginInit> {
    let region = kiro_region();
    let (client_id, client_secret) = register_kiro_client(&region).await?;
    let mut init = kiro_device_authorization(&region, &client_id, &client_secret).await?;
    init.account = account.to_string();
    init.display = display.to_string();
    Ok(init)
}

/// Poll the kiro token endpoint until the user completes the device login or
/// the flow errors out. Returns the populated [`DeviceAccountConfig`].
pub async fn poll_device_login(init: &DeviceLoginInit) -> Result<DeviceAccountConfig> {
    let url = token_endpoint(&init.provider);
    let client = reqwest::Client::new();
    let deadline = SystemTime::now() + Duration::from_secs(600);
    loop {
        if SystemTime::now() > deadline {
            anyhow::bail!("device login timed out");
        }
        // AWS SSO OIDC CreateToken expects a JSON body with camelCase keys
        // (mirrors OmniRoute); form-encoded snake_case yields 400
        // invalid_request once codewhisperer scopes are on the registration.
        let mut body = serde_json::json!({
            "grantType": "urn:ietf:params:oauth:grant-type:device_code",
            "deviceCode": init.device_code,
            "clientId": init.client_id
        });
        if !init.client_secret.is_empty() {
            body["clientSecret"] = serde_json::Value::String(init.client_secret.clone());
        }
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("device token poll")?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.context("parse token poll response")?;
        if status.is_success() {
            let access_token = v
                .get("accessToken")
                .or_else(|| v.get("access_token"))
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let refresh_token = v
                .get("refreshToken")
                .or_else(|| v.get("refresh_token"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let expires_at = v
                .get("expiresIn")
                .or_else(|| v.get("expires_in"))
                .and_then(|x| x.as_u64())
                .map(|s| SystemTime::now() + Duration::from_secs(s));
            // Derive the account identity directly from the signed-in provider
            // account. The token exchange response may already carry `email` /
            // `sub`; otherwise we fetch it from the provider's userinfo (or
            // decode the access-token JWT). No manual account_id is required.
            let identity = match v
                .get("email")
                .and_then(|x| x.as_str())
                .or_else(|| v.get("sub").and_then(|x| x.as_str()))
            {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => derive_identity(&init.provider, &access_token).await,
            };
            let mut cfg = DeviceAccountConfig {
                account_id: identity.clone(),
                display_name: Some(identity.clone()),
                provider: init.provider.clone(),
                access_token,
                refresh_token,
                expires_at,
                client_id: Some(init.client_id.clone()),
                client_secret: Some(init.client_secret.clone()),
                profile_arn: None,
            };
            // Best-effort profile ARN discovery (kiro only) + Builder ID token
            // prefix sanity check. Neither is fatal to the login.
            if let Some(arn) = discover_kiro_profile_arn(&cfg).await {
                cfg.profile_arn = Some(arn);
            }
            check_builder_id_token_prefix(&cfg);
            return Ok(cfg);
        }
        let err = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
        if err == "authorization_pending" {
            tokio::time::sleep(Duration::from_secs(init.interval.max(1))).await;
            continue;
        } else if err == "slow_down" {
            tokio::time::sleep(Duration::from_secs((init.interval * 2).max(1))).await;
            continue;
        }
        // Surface the raw OAuth error + description (no extra prefix — the
        // caller adds context; doubling reads as "failed: failed: ...").
        let desc = v
            .get("error_description")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if desc.is_empty() {
            anyhow::bail!("{}", err);
        }
        anyhow::bail!("{} — {}", err, desc);
    }
}

// ── Antigravity (Google) PKCE authorization-code flow ─────────────────────────

/// State for an in-progress Google PKCE login. The caller opens [`auth_url`] in
/// a browser; the loopback `redirect_uri` receives `?code=...`, which is passed
/// to [`complete_google_login`] together with this struct (which holds the
/// `code_verifier` needed to exchange the code).
#[derive(Debug, Clone)]
pub struct GoogleLoginInit {
    /// Fully-built Google consent URL (with PKCE challenge + redirect_uri).
    pub auth_url: String,
    /// PKCE code verifier (kept server-side; never sent to Google in the URL).
    pub code_verifier: String,
    /// Loopback redirect URI the browser is sent back to after consent.
    pub redirect_uri: String,
    /// CSRF/`state` value echoed back by Google and verified on callback.
    pub state: String,
}

/// Build the Google consent URL for the PKCE authorization-code flow.
///
/// `redirect_uri` must be a loopback URL (e.g. `http://localhost:3001/oauth/callback`)
/// that the caller is listening on. The returned [`GoogleLoginInit`] carries the
/// `code_verifier` + `state` needed to complete the exchange.
pub fn build_google_auth_url(redirect_uri: &str) -> GoogleLoginInit {
    let code_verifier = pkce_verifier();
    let code_challenge = pkce_challenge(&code_verifier);
    let state = random_url_safe(16);
    let scope = "https://www.googleapis.com/auth/cloud-platform \
                 https://www.googleapis.com/auth/userinfo.email \
                 https://www.googleapis.com/auth/userinfo.profile";
    let mut url = Url::parse(&google_auth_url()).expect("valid google auth url");
    {
        let mut q = url.query_pairs_mut();
        // Validate the client id is configured *before* building the URL so we
        // fail loudly (not by sending an empty client_id to Google).
        let client_id = google_client_id()
            .expect("XROUTER_GOOGLE_CLIENT_ID must be set before starting the antigravity login flow");
        q.append_pair("client_id", &client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("response_type", "code");
        q.append_pair("scope", scope);
        q.append_pair("access_type", "offline");
        q.append_pair("prompt", "consent");
        q.append_pair("code_challenge", &code_challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", &state);
    }
    GoogleLoginInit {
        auth_url: url.to_string(),
        code_verifier,
        redirect_uri: redirect_uri.to_string(),
        state,
    }
}

/// Exchange the `code` returned by Google's loopback redirect for tokens, derive
/// the account identity (email via userinfo), and return a populated
/// [`DeviceAccountConfig`].
///
/// `state` is the `state` query parameter echoed back by Google on the
/// loopback callback. It is verified against the value generated when the auth
/// URL was built (`init.state`); a mismatch is a CSRF attempt and fails the
/// login (mirrors OmniRoute's route-handler state verification).
pub async fn complete_google_login(
    init: &GoogleLoginInit,
    code: &str,
    state: &str,
) -> Result<DeviceAccountConfig> {
    if state != init.state {
        anyhow::bail!("invalid state");
    }
    let client = reqwest::Client::new();
    // Validate the client credentials are configured *before* performing the
    // exchange; otherwise we would silently send empty values to Google.
    let client_id = google_client_id()?;
    let client_secret = google_client_secret()?;
    let resp = client
        .post(google_token_url())
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code.to_string()),
            ("code_verifier", init.code_verifier.clone()),
            ("grant_type", "authorization_code".to_string()),
            ("redirect_uri", init.redirect_uri.clone()),
        ])
        .send()
        .await
        .context("google token exchange request")?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("google token exchange failed ({}): {}", status, txt);
    }
    let v: serde_json::Value = resp.json().await.context("parse google token response")?;
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
    // Google's token response does not include the email; fetch it from the
    // userinfo endpoint so we can key the account by email.
    let identity = match v
        .get("email")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_string(),
        None => derive_antigravity_identity(&access_token).await,
    };
    Ok(DeviceAccountConfig {
        account_id: identity.clone(),
        display_name: Some(identity.clone()),
        provider: "antigravity".to_string(),
        access_token,
        refresh_token,
        expires_at,
        client_id: Some(google_client_id()?),
        client_secret: Some(google_client_secret()?),
        profile_arn: None,
    })
}

// ── PKCE helpers ──────────────────────────────────────────────────────────────

/// Generate a 32-byte random PKCE code verifier, base64url (no padding).
fn pkce_verifier() -> String {
    base64_url_no_pad(&rand::random::<[u8; 32]>())
}

/// Derive the S256 PKCE code challenge from a verifier (SHA-256, base64url no
/// padding).
fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64_url_no_pad(&hasher.finalize())
}

/// `n` cryptographically-random bytes, base64url (no padding) — used for the
/// OAuth `state` and similar nonces.
///
/// Each byte is drawn independently from the OS CSPRNG via `rand::rng().fill`,
/// so the output has full `n`-byte entropy (the previous implementation cycled
/// a single 32-byte buffer, which was both lower-entropy and biased for
/// `n != 32`).
fn random_url_safe(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill(&mut bytes[..]);
    base64_url_no_pad(&bytes)
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.encode(bytes)
}

// ── identity derivation (no manual account_id required) ───────────────────────

/// Derive a stable account identity from the signed-in provider account.
///
/// * `antigravity` (Google): use `email`/`sub` from the token response if
///   present, otherwise call Google's userinfo endpoint to fetch `email`.
/// * `kiro` (AWS Builder ID): try the OIDC userinfo endpoint, otherwise decode
///   the access-token JWT `sub`/`email` claims (no signature verification),
///   falling back to `kiro-<first8(sha256(access_token))>`.
/// * generic `device:<name>`: `<name>-<first8(sha256(access_token))>`.
///
/// The returned string is used as both `account_id` and `display_name`, and as
/// the unique key (with `provider`) for [`DeviceStore::add_account`], so
/// re-login with the same provider account updates tokens instead of
/// duplicating.
pub async fn derive_identity(provider: &str, access_token: &str) -> String {
    match normalize_provider(provider) {
        "antigravity" => derive_antigravity_identity(access_token).await,
        "kiro" => derive_kiro_identity(access_token).await,
        other => format!("{}-{}", other, short_token_hash(access_token)),
    }
}

async fn derive_antigravity_identity(access_token: &str) -> String {
    match google_userinfo(access_token).await {
        Some(email) if !email.is_empty() => email,
        _ => format!("antigravity-{}", short_token_hash(access_token)),
    }
}

async fn google_userinfo(access_token: &str) -> Option<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(google_userinfo_endpoint())
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", access_token),
        )
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("email").and_then(|x| x.as_str()).map(|s| s.to_string())
}

async fn derive_kiro_identity(access_token: &str) -> String {
    // Prefer the OIDC userinfo endpoint for the registered client.
    if let Some(id) = kiro_userinfo(access_token).await {
        if !id.is_empty() {
            return id;
        }
    }
    // Fall back to decoding the access-token JWT claims. This decode is
    // unverified (see `decode_jwt_claims`); it is only used to read a
    // display/account-key identity, never for authorization.
    if let Some(claims) = decode_jwt_claims(access_token) {
        if let Some(email) = claims.get("email").and_then(|x| x.as_str()) {
            if !email.is_empty() {
                return email.to_string();
            }
        }
        if let Some(sub) = claims.get("sub").and_then(|x| x.as_str()) {
            if !sub.is_empty() {
                return sub.to_string();
            }
        }
    }
    format!("kiro-{}", short_token_hash(access_token))
}

async fn kiro_userinfo(access_token: &str) -> Option<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(kiro_userinfo_endpoint())
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", access_token),
        )
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("email")
        .and_then(|x| x.as_str())
        .or_else(|| v.get("sub").and_then(|x| x.as_str()))
        .map(|s| s.to_string())
}

fn kiro_userinfo_endpoint() -> String {
    format!("https://oidc.{}.amazonaws.com/userinfo", kiro_region())
}

/// Decode the payload of a JWT (base64url, **NO signature verification**) into
/// a JSON value. Returns `None` when the token is not a 3-part JWT or the
/// payload is not valid JSON.
///
/// SECURITY: this is an *unverified* decode. It is used only to derive a
/// non-secret, display/account-key identity (e.g. `email`/`sub`) when no
/// userinfo endpoint is reachable. The actual access token is always validated
/// upstream (by the provider when used as a `Bearer` credential, and by the
/// refresh flow), so a forged/unsigned JWT here can at worst produce a wrong
/// local account label — it can never grant access. Never use the result of
/// this function for any authorization decision.
fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = parts[1];
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// First 8 hex chars of sha256(token) — used as a stable, non-secret
/// fingerprint for the fallback identity.
fn short_token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    let mut s = String::new();
    for b in digest.iter().take(4) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ── provider-specific endpoints (env-overridable) ─────────────────────────────

/// Resolve the on-disk path of the device store.
///
/// Errors (rather than silently falling back to the current working directory)
/// when `HOME` is unset, because writing secrets into `./.local/share/...`
/// under an unexpected CWD could place them somewhere world-readable or
/// simply wrong.
fn store_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME is not set; cannot locate the device token store"))?;
    Ok(PathBuf::from(home).join(".local/share/xrouter/device-store.json"))
}

fn kiro_region() -> String {
    let region = std::env::var("KIRO_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    // Validate and fall back to the default region on a malformed value rather
    // than building broken AWS endpoints. Mirrors OmniRoute's region guard.
    if let Err(e) = assert_valid_aws_region(&region) {
        tracing::warn!("KIRO_REGION '{}' is invalid ({}); falling back to us-east-1", region, e);
        return "us-east-1".to_string();
    }
    region
}

fn kiro_register_endpoint(region: &str) -> String {
    std::env::var("XROUTER_KIRO_REGISTER_URL")
        .unwrap_or_else(|_| format!("https://oidc.{}.amazonaws.com/client/register", region))
}

fn kiro_device_auth_endpoint(region: &str) -> String {
    std::env::var("XROUTER_KIRO_DEVICE_AUTH_URL").unwrap_or_else(|_| {
        format!("https://oidc.{}.amazonaws.com/device_authorization", region)
    })
}

fn token_endpoint(provider: &str) -> String {
    match normalize_provider(provider) {
        "kiro" => format!("https://oidc.{}.amazonaws.com/token", kiro_region()),
        "antigravity" => google_token_url(),
        other => format!("https://{}.invalid/oauth/token", other),
    }
}

/// Google OAuth client id, from `XROUTER_GOOGLE_CLIENT_ID`.
///
/// Returns an error (not an empty string) when the variable is unset, so
/// callers surface a clear misconfiguration instead of silently sending an
/// empty `client_id` to Google and getting an opaque auth failure.
fn google_client_id() -> Result<String> {
    std::env::var("XROUTER_GOOGLE_CLIENT_ID").context(
        "XROUTER_GOOGLE_CLIENT_ID is required for the antigravity/Google login flow but is not set",
    )
}

/// Google OAuth client secret, from `XROUTER_GOOGLE_CLIENT_SECRET`.
///
/// Returns an error (not an empty string) when the variable is unset; see
/// [`google_client_id`] for rationale.
fn google_client_secret() -> Result<String> {
    std::env::var("XROUTER_GOOGLE_CLIENT_SECRET").context(
        "XROUTER_GOOGLE_CLIENT_SECRET is required for the antigravity/Google login flow but is not set",
    )
}

fn google_auth_url() -> String {
    std::env::var("XROUTER_GOOGLE_AUTH_URL")
        .unwrap_or_else(|_| "https://accounts.google.com/o/oauth2/v2/auth".to_string())
}

fn google_token_url() -> String {
    std::env::var("XROUTER_GOOGLE_TOKEN_URL")
        .unwrap_or_else(|_| "https://oauth2.googleapis.com/token".to_string())
}

fn google_userinfo_endpoint() -> String {
    std::env::var("XROUTER_GOOGLE_USERINFO_URL")
        .unwrap_or_else(|_| "https://www.googleapis.com/oauth2/v3/userinfo".to_string())
}

// ── Kiro scopes, region guard, profile ARN discovery, Builder ID checks ────────

/// Kiro (AWS Builder ID) OAuth scopes requested at client registration and
/// device authorization.
///
/// Defaults to `openid profile` plus the CodeWhisperer/Q Developer scopes
/// (mirrors OmniRoute). Override with the `KIRO_SCOPES` env var when a
/// different scope set is required.
fn kiro_scopes() -> String {
    std::env::var("KIRO_SCOPES").unwrap_or_else(|_| {
        "openid profile codewhisperer:completions codewhisperer:analysis codewhisperer:conversations"
            .to_string()
    })
}

/// Validate an AWS region string against `^[a-z]{2}-[a-z]+-\d$`.
///
/// Used as a guard in [`kiro_region`] and [`register_kiro_client`] so we never
/// build malformed `*.amazonaws.com` endpoints from a typo'd `KIRO_REGION`.
fn assert_valid_aws_region(region: &str) -> Result<()> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^[a-z]{2}-[a-z]+-\d$").expect("valid AWS region regex")
    });
    if re.is_match(region) {
        Ok(())
    } else {
        anyhow::bail!(
            "invalid AWS region '{}' (expected pattern ^[a-z]{{2}}-[a-z]+-\\d$)",
            region
        )
    }
}

/// Expected prefix for AWS Builder ID (kiro) access tokens. Builder ID tokens
/// are JWTs, so they begin with the JWT header prefix `eyJ`. If a kiro access
/// token does not start with this prefix we log a warning (non-fatal) — the
/// token may still be valid, but it is worth surfacing in case the wrong
/// credential was exchanged. Mirrors OmniRoute's Builder ID prefix sanity check.
const BUILDER_ID_TOKEN_PREFIX: &str = "eyJ";

/// Log a (non-fatal) warning when a Builder ID access token does not start with
/// the expected prefix. No-op for non-kiro providers.
fn check_builder_id_token_prefix(acct: &DeviceAccountConfig) {
    if normalize_provider(&acct.provider) != "kiro" {
        return;
    }
    if !acct.access_token.starts_with(BUILDER_ID_TOKEN_PREFIX) {
        tracing::warn!(
            "kiro (Builder ID) access token does not start with expected prefix '{}'; token may be invalid or mis-issued",
            BUILDER_ID_TOKEN_PREFIX
        );
    }
}

/// Best-effort discovery of the Q Developer / CodeWhisperer profile ARN for a
/// signed-in Builder ID account.
///
/// Mirrors OmniRoute's `discoverKiroProfileArnAcrossRegions`: probe the
/// profiles endpoint across the standard regions (`us-east-1`, `us-west-2`,
/// `eu-west-1`) and return the first `profileArn` found. This is purely
/// advisory — it never fails the login/refresh path; on any error we simply
/// return `None` and the caller leaves `profile_arn` unset.
pub async fn discover_kiro_profile_arn(acct: &DeviceAccountConfig) -> Option<String> {
    if normalize_provider(&acct.provider) != "kiro" {
        return None;
    }
    const REGIONS: &[&str] = &["us-east-1", "us-west-2", "eu-west-1"];
    let client = reqwest::Client::new();
    for region in REGIONS {
        let url = format!("https://codewhisperer.{}.amazonaws.com/", region);
        let resp = match client
            .get(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", acct.access_token),
            )
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !resp.status().is_success() {
            continue;
        }
        let v: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => continue,
        };
        // Accept either a top-level `profileArn` or a `profiles[]` list.
        if let Some(arn) = v.get("profileArn").and_then(|x| x.as_str()) {
            if !arn.is_empty() {
                return Some(arn.to_string());
            }
        }
        if let Some(arr) = v.get("profiles").and_then(|x| x.as_array()) {
            if let Some(arn) = arr
                .iter()
                .find_map(|p| p.get("profileArn").and_then(|x| x.as_str()))
            {
                if !arn.is_empty() {
                    return Some(arn.to_string());
                }
            }
        }
    }
    None
}

/// Helper: current unix-epoch seconds (used by callers that log expiry).
#[allow(dead_code)]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_is_url_safe_no_padding() {
        let v = pkce_verifier();
        assert!(!v.ends_with('='));
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        // challenge must be deterministic for a given verifier
        assert_eq!(pkce_challenge(&v), pkce_challenge(&v));
        // and differ from the verifier
        assert_ne!(v, pkce_challenge(&v));
    }

    #[test]
    fn google_auth_url_matches_spec() {
        std::env::set_var(
            "XROUTER_GOOGLE_CLIENT_ID",
            "test-client-id.apps.googleusercontent.com",
        );
        std::env::set_var("XROUTER_GOOGLE_CLIENT_SECRET", "test-client-secret");
        let init = build_google_auth_url("http://localhost:3001/oauth/callback");
        let url = Url::parse(&init.auth_url).expect("valid url");
        let q: HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(url.origin().ascii_serialization(), "https://accounts.google.com");
        assert_eq!(url.path(), "/o/oauth2/v2/auth");
        assert_eq!(q.get("client_id").unwrap(), &google_client_id().unwrap());
        assert_eq!(
            q.get("redirect_uri").unwrap(),
            "http://localhost:3001/oauth/callback"
        );
        assert_eq!(q.get("response_type").unwrap(), "code");
        assert_eq!(q.get("access_type").unwrap(), "offline");
        assert_eq!(q.get("prompt").unwrap(), "consent");
        assert_eq!(q.get("code_challenge_method").unwrap(), "S256");
        let scope = q.get("scope").unwrap();
        assert!(scope.contains("https://www.googleapis.com/auth/cloud-platform"));
        assert!(scope.contains("https://www.googleapis.com/auth/userinfo.email"));
        assert!(scope.contains("https://www.googleapis.com/auth/userinfo.profile"));
        // challenge must verify against the verifier
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(init.code_verifier.as_bytes());
        let expected = base64_url_no_pad(&h.finalize());
        assert_eq!(q.get("code_challenge").unwrap(), &expected);
        assert!(!init.state.is_empty());
    }

    #[test]
    fn kiro_endpoints_use_region() {
        std::env::set_var("KIRO_REGION", "eu-west-1");
        assert_eq!(
            kiro_register_endpoint("eu-west-1"),
            "https://oidc.eu-west-1.amazonaws.com/client/register"
        );
        assert_eq!(
            kiro_device_auth_endpoint("eu-west-1"),
            "https://oidc.eu-west-1.amazonaws.com/device_authorization"
        );
        assert_eq!(
            token_endpoint("kiro"),
            "https://oidc.eu-west-1.amazonaws.com/token"
        );
        std::env::remove_var("KIRO_REGION");
    }

    #[test]
    fn valid_aws_region_patterns() {
        assert!(assert_valid_aws_region("us-east-1").is_ok());
        assert!(assert_valid_aws_region("eu-west-2").is_ok());
        assert!(assert_valid_aws_region("ap-southeast-1").is_ok());
        assert!(assert_valid_aws_region("us-east").is_err());
        assert!(assert_valid_aws_region("US-east-1").is_err());
        assert!(assert_valid_aws_region("us-east-12").is_err());
        assert!(assert_valid_aws_region("").is_err());
    }

    #[test]
    fn grant_expired_error_detection() {
        assert!(is_grant_expired_error("foo InvalidGrantException bar"));
        assert!(is_grant_expired_error("ExpiredTokenException"));
        assert!(is_grant_expired_error("error: invalid_grant"));
        assert!(!is_grant_expired_error("some other error"));
        assert!(!is_grant_expired_error(""));
    }

    #[test]
    fn kiro_scopes_default_and_env() {
        std::env::remove_var("KIRO_SCOPES");
        let s = kiro_scopes();
        assert!(s.contains("openid"));
        assert!(s.contains("codewhisperer:completions"));
        assert!(s.contains("codewhisperer:analysis"));
        assert!(s.contains("codewhisperer:conversations"));
        std::env::set_var("KIRO_SCOPES", "openid custom");
        assert_eq!(kiro_scopes(), "openid custom");
        std::env::remove_var("KIRO_SCOPES");
    }

    #[test]
    fn google_login_state_verification() {
        // A mismatched callback state must be rejected (CSRF guard).
        let init = GoogleLoginInit {
            auth_url: String::new(),
            code_verifier: String::new(),
            redirect_uri: String::new(),
            state: "expected".to_string(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        // We can't complete a real exchange, but we can assert the state check
        // short-circuits before any network call by supplying a bad state.
        let err = rt.block_on(complete_google_login(&init, "code", "wrong"));
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().to_string(), "invalid state");
    }
}
