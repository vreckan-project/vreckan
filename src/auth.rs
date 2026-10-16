//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Web-login authentication: opaque server-issued tokens, the auth cookie, and
//! the token/admin verification used to gate API routes.
//!
//! Ports the auth-token machinery in `server/api` (`AUTH_TOKENS`,
//! `issue_auth_token`, `revoke_auth_token`, `revoke_all_tokens_for_user`,
//! `save_auth_tokens_to_disk`, `load_auth_tokens_from_disk`,
//! `auth_cookie_kwargs`, `verify_token`, `verify_admin`) and the auth routes in
//! `server/routers/auth`.
//!
//! The web-login credential is an opaque, random token (not a JWT). It is
//! stored in the `auth_tokens` table (durable across restarts) and mirrored in
//! an in-memory map for fast per-request lookups. The token travels either in
//! the `vreckan_auth` HttpOnly cookie (browser flows) or a `Bearer` header
//! (explicit clients). The OIDC/SSO flow (group C) issues the same token.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{OriginalUri, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine;
use jwks::Jwks;
use rand::Rng;
use sea_orm::{ActiveModelTrait, ActiveValue, ConnectionTrait, DbErr, EntityTrait};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::entities;
use crate::permissions;
use crate::settings::{DefaultDbCredsPolicy, Settings};
use crate::state::AppState;
use crate::user_manager::{
    self, db_save_group, db_save_user_row, get_group_row, UserError, UserRecord,
};

/// The current wall-clock time as a Unix timestamp in seconds.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Generate an opaque, URL-safe token (mirrors Python `secrets.token_urlsafe(32)`):
/// 32 random bytes, base64url-encoded without padding.
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Token store
// ---------------------------------------------------------------------------

/// Issue a new opaque web-login token for a user and store it (in memory and,
/// via the caller, in the database). Returns the token string.
pub async fn issue_auth_token(state: &AppState, username: &str) -> Result<String, UserError> {
    let token = generate_token();
    let expires_at = now_secs() + state.settings.auth_token_ttl_seconds as f64;
    state
        .auth_tokens
        .write()
        .await
        .insert(token.clone(), crate::state::AuthTokenRecord {
            username: username.to_string(),
            expires_at,
        });
    Ok(token)
}

/// Revoke a single token (no-op if absent).
pub async fn revoke_auth_token(state: &AppState, token: &str) {
    if !token.is_empty() {
        state.auth_tokens.write().await.remove(token);
    }
}

/// Revoke ALL active web-login tokens belonging to a user. Returns the number
/// revoked. Used by the self-revoke and admin force-logout endpoints.
pub async fn revoke_all_tokens_for_user(state: &AppState, username: &str) -> usize {
    let mut map = state.auth_tokens.write().await;
    let to_revoke: Vec<String> = map
        .iter()
        .filter(|(_, v)| v.username == username)
        .map(|(t, _)| t.clone())
        .collect();
    for t in &to_revoke {
        map.remove(t);
    }
    to_revoke.len()
}

/// Persist the in-memory token store to the database (atomic replace), pruning
/// expired entries. Mirrors `save_auth_tokens_to_disk`.
pub async fn save_auth_tokens(state: &AppState) -> Result<(), DbErr> {
    let conn = &state.db.conn;
    let snapshot: Vec<(String, String, f64)> = {
        let map = state.auth_tokens.read().await;
        let now = now_secs();
        map.iter()
            .filter(|(_, v)| v.expires_at > now)
            .map(|(t, v)| (t.clone(), v.username.clone(), v.expires_at))
            .collect()
    };
    // Replace the whole table: delete all, then insert the snapshot.
    let backend = conn.get_database_backend();
    conn.execute_raw(
        sea_orm::Statement::from_string(backend, "DELETE FROM auth_tokens"),
    )
    .await?;
    for (token, username, expires_at) in snapshot {
        let am = entities::auth_token::ActiveModel {
            token: ActiveValue::Set(token),
            username: ActiveValue::Set(username),
            expires_at: ActiveValue::Set(expires_at),
        };
        am.insert(conn).await?;
    }
    Ok(())
}

/// Load the web-login token store from the database into memory, pruning
/// expired entries. Called once at startup. Mirrors `load_auth_tokens_from_disk`.
pub async fn load_auth_tokens(state: &AppState) -> Result<(), DbErr> {
    let conn = &state.db.conn;
    let rows = entities::auth_token::Entity::find().all(conn).await?;
    let now = now_secs();
    let mut map = state.auth_tokens.write().await;
    map.clear();
    for r in rows {
        if r.expires_at > now {
            map.insert(
                r.token,
                crate::state::AuthTokenRecord {
                    username: r.username,
                    expires_at: r.expires_at,
                },
            );
        }
    }
    info!("Loaded {} active web-login token(s) from the database.", map.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Auth cookie
// ---------------------------------------------------------------------------

/// Build the `Set-Cookie` header value for the web-login cookie. When `token`
/// is None the cookie is cleared (Max-Age=0). The `Secure` flag mirrors the
/// request scheme (enabled on HTTPS, disabled on plain HTTP so the cookie is
/// usable in local/test environments).
pub fn auth_cookie(settings: &Settings, token: Option<&str>, secure: bool) -> String {
    let max_age = if token.is_some() {
        settings.auth_token_ttl_seconds
    } else {
        0
    };
    let value = token.unwrap_or("");
    let mut s = format!(
        "{}={}; Max-Age={}; Path=/; HttpOnly; SameSite=Lax",
        settings.auth_cookie_name, value, max_age
    );
    if secure {
        s.push_str("; Secure");
    }
    s
}

/// The request's effective scheme, honoring the `X-Forwarded-Proto` header
/// (set by a reverse proxy) and falling back to `https` (the server always
/// terminates TLS).
pub fn request_scheme(headers: &axum::http::HeaderMap) -> &str {
    if let Some(v) = headers.get(axum::http::HeaderName::from_static("x-forwarded-proto")) {
        if let Ok(s) = v.to_str() {
            return s;
        }
    }
    "https"
}

// ---------------------------------------------------------------------------
// Token / admin verification
// ---------------------------------------------------------------------------

/// A resolved, authenticated user: the account record plus the effective
/// permissions and settings computed for this request. Mirrors the user dict
/// returned by Python's `verify_token` (with `effective_settings`,
/// `permissions`, `roles`, `groups` attached).
#[derive(Clone, Debug, Serialize)]
pub struct AuthenticatedUser {
    pub record: UserRecord,
    /// The user's effective (expanded) permission set.
    pub permissions: Vec<String>,
    /// The user's effective (merged) settings.
    pub effective_settings: serde_json::Value,
}

impl AuthenticatedUser {
    /// True if the user's effective permissions include the `admin`
    /// super-permission.
    pub fn is_admin(&self) -> bool {
        self.permissions
            .iter()
            .any(|p| p == permissions::SUPER_PERMISSION)
    }
}

/// Extract the web-login token from a request: the `vreckan_auth` cookie, or a
/// `Bearer` header (only if it is a token we issued).
async fn extract_token(state: &AppState, headers: &axum::http::HeaderMap) -> Option<String> {
    // Cookie first.
    if let Some(cookie_header) = headers.get(header::COOKIE) {
        if let Ok(cookie_str) = cookie_header.to_str() {
            let name = &state.settings.auth_cookie_name;
            for part in cookie_str.split(';') {
                let kv = part.trim();
                if let Some((k, v)) = kv.split_once('=') {
                    if k == name && !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    // Bearer header (only if it is a token we issued).
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth.to_str() {
            if let Some(bearer) = auth_str.strip_prefix("Bearer ") {
                let bearer = bearer.trim();
                if state.auth_tokens.read().await.contains_key(bearer) {
                    return Some(bearer.to_string());
                }
            }
        }
    }
    None
}

/// Resolve a server-issued opaque web-login token to an authenticated user.
/// Returns None if the token is absent, unknown, or expired.
async fn user_from_token(
    state: &AppState,
    token: Option<&str>,
) -> Result<Option<AuthenticatedUser>, UserError> {
    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(None),
    };
    let entry = state.auth_tokens.read().await.get(token).cloned();
    let entry = match entry {
        Some(e) => e,
        None => return Ok(None),
    };
    if entry.expires_at < now_secs() {
        return Ok(None);
    }
    let record = match user_manager::get_user(&state.db, &entry.username).await? {
        Some(u) => u,
        None => return Ok(None),
    };
    let effective_settings = user_manager::get_effective_settings(&state.db, &entry.username).await?;
    // No admin bypass: an individually disabled account is rejected here,
    // exactly like any other inactive user.
    if !effective_settings.get("active").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(UserError::Validation(
            "User account is inactive.".to_string(),
        ));
    }
    let permissions = user_manager::get_effective_permissions(&state.db, &entry.username).await?;
    Ok(Some(AuthenticatedUser {
        record,
        permissions,
        effective_settings,
    }))
}

/// Verify the caller's web-login token (cookie or Bearer). Returns the
/// authenticated user, or a 401-mapped error if not authenticated.
pub async fn verify_token(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<AuthenticatedUser, UserError> {
    let token = extract_token(state, headers).await;
    match user_from_token(state, token.as_deref()).await? {
        Some(u) => Ok(u),
        None => Err(UserError::Unauthorized("Not authenticated.".to_string())),
    }
}

/// Verify the caller is an admin (their effective permissions include the
/// `admin` super-permission).
pub async fn verify_admin(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<AuthenticatedUser, UserError> {
    let user = verify_token(state, headers).await?;
    if !user.is_admin() {
        return Err(UserError::Validation(
            "Admin privileges required.".to_string(),
        ));
    }
    Ok(user)
}

// ---------------------------------------------------------------------------
// Auth routes (group B)
//
// Ports the non-OIDC routes in `server/routers/auth`: `/login`, `/logout`,
// `/revoke-sessions`, `/me`, `/change_password`, `/set_password`,
// `/preferences`. These live on the *plaintext* auth router (outside the
// E2EE-encrypted router) because the browser reaches them in clear text before
// any session exists. The OIDC/SSO routes (group C) are added to the same
// router.
// ---------------------------------------------------------------------------

/// Body for `POST /api/auth/login`.
#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Body for `POST /api/auth/change_password`.
#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

/// Body for `POST /api/auth/set_password`.
#[derive(Deserialize)]
pub struct SetPasswordRequest {
    pub password: String,
}

/// Query for `POST /api/auth/set_password` (optional target username).
#[derive(Deserialize)]
pub struct SetPasswordQuery {
    pub target: Option<String>,
}

/// Body for `PUT /api/auth/preferences` — the caller's personal UI prefs.
#[derive(Deserialize)]
pub struct UiPreferencesRequest {
    #[serde(default)]
    pub ui_language: Option<String>,
}

/// Optional body for `POST /api/auth/revoke-sessions`. If a username is
/// supplied it must match the authenticated caller.
#[derive(Deserialize)]
pub struct RevokeOwnSessionsRequest {
    #[serde(default)]
    pub username: Option<String>,
}

/// Response for `POST /api/auth/revoke-sessions`.
#[derive(Serialize)]
pub struct RevokeOidcSessionResponse {
    pub username: String,
    pub revoked: usize,
}

/// The caller's account metadata (mirrors Python `MeResponse`).
#[derive(Serialize)]
pub struct MeResponse {
    pub username: String,
    pub is_admin: bool,
    pub active: bool,
    pub has_password: bool,
    pub is_sso: bool,
    /// True while the account still carries its initial (bootstrap)
    /// password; the client should force a password change on first login.
    pub must_change_password: bool,
    /// The user's effective (merged) settings object.
    pub settings: serde_json::Value,
    /// The user's effective (expanded) permission set.
    pub permissions: Vec<String>,
    pub roles: Vec<String>,
    pub groups: Vec<String>,
}

/// Build a `MeResponse` from a user record plus the effective settings and the
/// permission list to report. Mirrors Python `_me_response`.
async fn me_response(
    state: &AppState,
    user: &UserRecord,
    effective: &serde_json::Value,
    permissions: Vec<String>,
) -> MeResponse {
    // No admin bypass in the reported `active` flag: it reflects the effective
    // setting so the UI sees the true state, including a disabled admin.
    let active = effective
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let has_pw = user_manager::has_password(&state.db, &user.username)
        .await
        .unwrap_or(false);
    MeResponse {
        username: user.username.clone(),
        is_admin: user.is_admin,
        active,
        has_password: has_pw,
        is_sso: user.is_sso,
        must_change_password: user.must_change_password,
        settings: effective.clone(),
        permissions,
        roles: user.roles.clone(),
        groups: user.groups.clone(),
    }
}

/// Build a JSON error response with a `detail` field (mirrors FastAPI's
/// `HTTPException`).
fn json_error(status: StatusCode, detail: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "detail": detail })),
    )
        .into_response()
}

/// Map a `UserError` to a JSON error response using its HTTP status.
fn user_error_response(e: UserError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(serde_json::json!({ "detail": e.to_string() })),
    )
        .into_response()
}

/// The client's remote address as seen by the server: the `ConnectInfo`
/// extension added by `into_make_service_with_connect_info` in `main.rs`, or
/// `None` (unproxied local clients and direct-router test harnesses). Infallible
/// so the handler works on a plain router too.
#[derive(Clone, Copy, Debug, Default)]
struct RemoteAddr(Option<std::net::SocketAddr>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for RemoteAddr {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|c| c.0),
        ))
    }
}

/// Whether the direct TCP peer is one of the configured trusted reverse
/// proxies (`trusted_proxies`; ANT-276, the same control as H4 for
/// `X-Forwarded-Proto`). Only then are `X-Forwarded-*` headers honoured: a
/// client reaching the server directly can otherwise rotate the headers at
/// will. Public so the remaining H4 call sites can reuse the same gate.
pub fn peer_is_trusted_proxy(
    settings: &Settings,
    remote: Option<std::net::SocketAddr>,
) -> bool {
    let Some(ip) = remote.map(|a| a.ip()) else {
        return false;
    };
    settings
        .trusted_proxies
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .any(|entry| entry_matches_ip(entry, ip))
}

/// Match one `trusted_proxies` entry — a bare IP or an `IP/prefix` CIDR —
/// against a peer address. Malformed entries never match.
fn entry_matches_ip(entry: &str, ip: std::net::IpAddr) -> bool {
    match entry.split_once('/') {
        Some((network, prefix)) => {
            let Ok(prefix) = prefix.parse::<u32>() else {
                return false;
            };
            if prefix > 128 {
                return false;
            }
            let Ok(network) = network.parse::<std::net::IpAddr>() else {
                return false;
            };
            ip_in_network(ip, network, prefix as u8)
        }
        None => entry.parse::<std::net::IpAddr>() == Ok(ip),
    }
}

/// True when `ip` lies inside the `network/prefix` block (v4 and v6; a
/// version mismatch never matches).
fn ip_in_network(ip: std::net::IpAddr, network: std::net::IpAddr, prefix: u8) -> bool {
    match (ip, network) {
        (std::net::IpAddr::V4(a), std::net::IpAddr::V4(b)) => {
            let (a, b) = (u32::from(a), u32::from(b));
            if prefix == 0 {
                return true;
            }
            let mask = u32::MAX << (32 - prefix);
            (a & mask) == (b & mask)
        }
        (std::net::IpAddr::V6(a), std::net::IpAddr::V6(b)) => {
            let (a, b) = (u128::from(a), u128::from(b));
            if prefix == 0 {
                return true;
            }
            let mask = u128::MAX << (128 - prefix);
            (a & mask) == (b & mask)
        }
        _ => false,
    }
}

/// The client's IP for login rate limiting: the leftmost `X-Forwarded-For`
/// entry when the direct peer is a configured trusted proxy (the proxy
/// appends the entry, so the client cannot forge it), otherwise the direct
/// connection address, then to a shared `"unknown"` bucket (unproxied local
/// clients and test harnesses). An XFF header from an untrusted peer is
/// ignored: a rotating value would defeat the per-IP lockout (ANT-276).
fn client_ip(
    settings: &Settings,
    headers: &HeaderMap,
    remote: Option<std::net::SocketAddr>,
) -> String {
    if peer_is_trusted_proxy(settings, remote) {
        if let Some(v) = headers.get(axum::http::HeaderName::from_static("x-forwarded-for")) {
            if let Ok(s) = v.to_str() {
                if let Some(first) = s.split(',').next() {
                    let first = first.trim();
                    if !first.is_empty() {
                        return first.to_string();
                    }
                }
            }
        }
    }
    if let Some(addr) = remote {
        return addr.ip().to_string();
    }
    "unknown".to_string()
}

/// Record a failed login for the (IP, username) pair, starting (and logging)
/// a lockout once the pair's failure count reaches the configured maximum.
async fn record_login_failure(
    state: &AppState,
    key: &str,
    now: f64,
    window_secs: f64,
    max_attempts: i64,
) {
    let mut map = state.login_attempts.write().await;
    let w = map.entry(key.to_string()).or_default();
    if w.window_start == 0.0 {
        w.window_start = now;
    }
    w.fails += 1;
    if max_attempts > 0 && w.fails >= max_attempts as u32 {
        w.locked_until = now + window_secs;
        warn!(
            "login rate limit: {} locked out for {}s after {} failed attempts",
            key,
            window_secs as i64,
            w.fails
        );
    }
}

/// `POST /api/auth/login` — authenticate with a username/password and issue a
/// server token. Sets an HttpOnly cookie (and returns the token in the
/// `X-Auth-Token` header) so both browser cookie flows and explicit Bearer
/// clients work.
///
/// Brute-force protection: per (client IP, username) pair, `login_max_attempts`
/// failed attempts within the `login_lockout_seconds` window lock the pair
/// out for the same window (fail closed with 429). The miss that reaches the
/// threshold is a plain 401 like any other miss; every attempt after it —
/// correct password included — gets 429 until the lockout elapses. The window
/// resets rather than compounds, and a successful login clears the pair.
async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    RemoteAddr(remote): RemoteAddr,
    Json(req): Json<LoginRequest>,
) -> Response {
    let ip = client_ip(&state.settings, &headers, remote);
    // Lowercase the username in the key: 'Admin' and 'admin' are the same
    // account for lockout-budget purposes (ANT-276).
    let key = format!("{ip}:{}", req.username.to_ascii_lowercase());
    let now = now_secs();
    let max_attempts = state.settings.login_max_attempts;
    let window_secs = state.settings.login_lockout_seconds.max(1) as f64;
    if max_attempts > 0 {
        let mut map = state.login_attempts.write().await;
        // Opportunistic prune so the map stays bounded on long-lived servers.
        if map.len() > 4096 {
            map.retain(|_, w| w.locked_until > now || now - w.window_start < window_secs);
        }
        let w = match map.get_mut(&key) {
            Some(w) => w,
            None => {
                map.insert(key.clone(), crate::state::LoginAttemptWindow { window_start: now, ..Default::default() });
                map.get_mut(&key).unwrap()
            }
        };
        if w.window_start == 0.0 || now - w.window_start > window_secs {
            w.fails = 0;
            w.window_start = now;
        }
        if w.locked_until > now {
            debug!("login rate limit: {} still locked out (until +{}s)", key, (w.locked_until - now) as i64);
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many failed login attempts; try again in a few minutes.",
            );
        }
        if w.fails >= max_attempts as u32 {
            w.locked_until = now + window_secs;
            warn!(
                "login rate limit: {} locked out for {}s after {} failed attempts",
                key,
                window_secs as i64,
                w.fails
            );
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many failed login attempts; try again in a few minutes.",
            );
        }
    }
    // argon2 is a hard dependency in the Rust port, so the Python
    // `_HAS_ARGON2` 503 check is always satisfied here.
    let ok = user_manager::verify_password(&state.db, &req.username, &req.password)
        .await
        .unwrap_or(false);
    if !ok {
        record_login_failure(&state, &key, now, window_secs, max_attempts).await;
        // Do not leak whether the account exists.
        return json_error(StatusCode::UNAUTHORIZED, "Invalid username or password.");
    }
    if max_attempts > 0 {
        state.login_attempts.write().await.remove(&key);
    }
    let user = match user_manager::get_user(&state.db, &req.username).await {
        Ok(Some(u)) => u,
        _ => return json_error(StatusCode::UNAUTHORIZED, "Invalid username or password."),
    };
    // The default-password policy applies only while the account still
    // carries its initial (bootstrap) password: the login succeeded, so the
    // presented password is by definition the initial one.
    if user.must_change_password
        && req.password == user_manager::DEFAULT_BOOTSTRAP_PASSWORD
    {
        let policy = state.settings.default_password_policy.to_lowercase();
        match policy.as_str() {
            "allow" => {}
            "reject" => {
                // The specific body tells an unauthenticated caller that the
                // default bootstrap password is still in use (a login-body
                // oracle, ANT-276). Show it only when the operator explicitly
                // opted into running on defaults (silent); under the default
                // warn policy collapse to the generic 401 body.
                if state.settings.default_db_creds_policy == DefaultDbCredsPolicy::Silent {
                    return json_error(
                        StatusCode::UNAUTHORIZED,
                        "Still using the default bootstrap password; change it.",
                    );
                }
                return json_error(StatusCode::UNAUTHORIZED, "Invalid username or password.");
            }
            _ => warn!(
                "User '{}' logged in with the default bootstrap password (policy: warn).",
                req.username
            ),
        }
    }
    let effective = match user_manager::get_effective_settings(&state.db, &req.username).await {
        Ok(e) => e,
        Err(e) => return user_error_response(e),
    };
    // No admin bypass: a disabled admin cannot log in, same as any other
    // inactive account.
    let is_active = effective
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if !is_active {
        return json_error(StatusCode::FORBIDDEN, "This account is not active.");
    }
    let token = match issue_auth_token(&state, &req.username).await {
        Ok(t) => t,
        Err(e) => return user_error_response(e),
    };
    if let Err(e) = save_auth_tokens(&state).await {
        return user_error_response(e.into());
    }
    // Report the user's direct permissions if they have any, otherwise the
    // effective (expanded) set — mirroring Python `_me_response`.
    let perms = if user.permissions.is_empty() {
        user_manager::get_effective_permissions(&state.db, &req.username)
            .await
            .unwrap_or_default()
    } else {
        user.permissions.clone()
    };
    let me = me_response(&state, &user, &effective, perms).await;
    let secure = request_scheme(&headers) == "https";
    let cookie = auth_cookie(&state.settings, Some(&token), secure);
    (
        [
            (header::SET_COOKIE, cookie),
            (axum::http::HeaderName::from_static("x-auth-token"), token),
        ],
        Json(me),
    )
        .into_response()
}

/// `POST /api/auth/logout` — revoke the caller's server token (cookie or
/// Bearer) and clear the cookie.
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = extract_token(&state, &headers).await {
        revoke_auth_token(&state, &token).await;
        let _ = save_auth_tokens(&state).await;
    }
    let secure = request_scheme(&headers) == "https";
    let cookie = auth_cookie(&state.settings, None, secure);
    (
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "message": "Logged out" })),
    )
        .into_response()
}

/// `POST /api/auth/revoke-sessions` — revoke ALL of the caller's own web
/// sessions (self-service). The target is always the authenticated caller; if
/// a different username is supplied in the body the request is rejected (403).
async fn revoke_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => return user_error_response(e),
    };
    let caller = user.record.username.clone();
    // Optional body. If a username is supplied it must match the caller.
    if !body.is_empty() {
        match serde_json::from_slice::<RevokeOwnSessionsRequest>(&body) {
            Ok(req) => {
                if let Some(ref name) = req.username {
                    if !name.is_empty() && name != &caller {
                        warn!(
                            "User '{caller}' attempted to revoke sessions for '{name}' — rejected (self-revoke only)."
                        );
                        return json_error(
                            StatusCode::FORBIDDEN,
                            "You can only invalidate your own sessions.",
                        );
                    }
                }
            }
            Err(e) => {
                return json_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &format!("Invalid request body: {e}"),
                )
            }
        }
    }
    let revoked = revoke_all_tokens_for_user(&state, &caller).await;
    let _ = save_auth_tokens(&state).await;
    info!(
        "Self-revoke: user '{caller}' revoked {revoked} of their own web token(s)."
    );
    let secure = request_scheme(&headers) == "https";
    let cookie = auth_cookie(&state.settings, None, secure);
    (
        [(header::SET_COOKIE, cookie)],
        Json(RevokeOidcSessionResponse {
            username: caller,
            revoked,
        }),
    )
        .into_response()
}

/// `GET /api/auth/me` — return the caller's account metadata.
async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => return user_error_response(e),
    };
    let me = me_response(
        &state,
        &user.record,
        &user.effective_settings,
        user.permissions.clone(),
    )
    .await;
    Json(me).into_response()
}

/// `POST /api/auth/change_password` — let the caller change their own password
/// (verifies the old one first).
async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordRequest>,
) -> Response {
    // Mirrors the Pydantic `min_length=8` on `new_password` (a 422 validation
    // error raised before the handler body runs).
    if req.new_password.len() < 8 {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "new_password is too short (min 8).",
        );
    }
    let user = match verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => return user_error_response(e),
    };
    let username = user.record.username.clone();
    if !user_manager::verify_password(&state.db, &username, &req.old_password)
        .await
        .unwrap_or(false)
    {
        return json_error(StatusCode::UNAUTHORIZED, "Current password is incorrect.");
    }
    if let Err(e) = user_manager::set_password(&state.db, &username, &req.new_password).await {
        return user_error_response(e);
    }
    Json(serde_json::json!({ "message": "Password updated." })).into_response()
}

/// `POST /api/auth/set_password` — admin: set or reset the password for a user
/// (or oneself, via the optional `target` query param).
async fn set_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SetPasswordQuery>,
    Json(req): Json<SetPasswordRequest>,
) -> Response {
    // Mirrors the Pydantic `min_length=8` on `password` (a 422 validation
    // error raised before the handler body runs).
    if req.password.len() < 8 {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "password is too short (min 8).",
        );
    }
    let admin = match verify_admin(&state, &headers).await {
        Ok(u) => u,
        Err(e) => return user_error_response(e),
    };
    let username = q.target.unwrap_or_else(|| admin.record.username.clone());
    if user_manager::get_user(&state.db, &username)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return json_error(StatusCode::NOT_FOUND, "User not found.");
    }
    if let Err(e) = user_manager::set_password(&state.db, &username, &req.password).await {
        return user_error_response(e);
    }
    Json(serde_json::json!({
        "message": format!("Password set for '{username}'."),
    }))
    .into_response()
}

/// `PUT /api/auth/preferences` — persist the caller's personal UI preferences
/// (currently: UI language). Only the provided fields are touched
/// (read-modify-write), so other settings are preserved.
async fn preferences(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UiPreferencesRequest>,
) -> Response {
    let user = match verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => return user_error_response(e),
    };
    let username = user.record.username.clone();
    let current = match user_manager::get_user(&state.db, &username).await {
        Ok(Some(u)) => u,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "User not found."),
        Err(e) => return user_error_response(e),
    };
    let mut stored = current
        .settings
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(ref lang) = req.ui_language {
        stored["ui_language"] = serde_json::Value::String(lang.clone());
    }
    if let Err(e) = user_manager::update_user_settings(&state.db, &username, stored).await {
        return user_error_response(e);
    }
    // Re-fetch so the response reflects the value we just saved.
    let updated = match user_manager::get_user(&state.db, &username).await {
        Ok(Some(u)) => u,
        _ => current,
    };
    let effective = match user_manager::get_effective_settings(&state.db, &username).await {
        Ok(e) => e,
        Err(e) => return user_error_response(e),
    };
    let perms = if updated.permissions.is_empty() {
        user_manager::get_effective_permissions(&state.db, &username)
            .await
            .unwrap_or_default()
    } else {
        updated.permissions.clone()
    };
    let me = me_response(&state, &updated, &effective, perms).await;
    Json(me).into_response()
}

/// Build the auth router (mounted at `/api/auth`).
pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/revoke-sessions", post(revoke_sessions))
        .route("/me", get(me))
        .route("/change_password", post(change_password))
        .route("/set_password", post(set_password))
        .route("/preferences", put(preferences))
        // OIDC / SSO (authorization-code flow, no client-side secrets).
        .route("/oidc/status", get(oidc_status))
        .route("/oidc/authorize", get(oidc_authorize))
        .route("/oidc/callback", get(oidc_callback))
}

// ---------------------------------------------------------------------------
// OIDC / SSO (group C)
//
// Ports the OIDC helpers and routes in `server/routers/auth`:
// `_oidc_config`, `_oidc_effective_issuer`, `_oidc_base_url`,
// `_oidc_redirect_uri`, `_oidc_discovery`, `_oidc_verify_id_token`,
// `_oidc_username_from_claims`, `_oidc_admin_groups_set`,
// `_oidc_user_groups_set`, `_oidc_apply_groups`, and the `oidc_status` /
// `oidc_authorize` / `oidc_callback` routes.
//
// These endpoints are on the *plaintext* auth router (like `/login`) because
// the browser reaches them in clear text before any E2EE session exists, and
// they never expose a client secret. They set the SAME `vreckan_auth` web
// token cookie that password login sets.
// ---------------------------------------------------------------------------

/// The effective OIDC/SSO settings, read DB-first (a value saved on the admin
/// SSO page overrides the environment, which overrides the default).
pub struct OidcConfig {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub scopes: String,
    pub allow_signup: bool,
    pub state_ttl: i64,
    pub admin_groups: String,
    pub user_groups: String,
    pub require_signature: bool,
}

/// The effective OIDC/SSO settings for a request (DB-over-env-over-default).
/// Public so the admin SSO endpoints (and tests) can read the same resolved
/// values the auth flow uses.
pub async fn oidc_config(state: &AppState) -> OidcConfig {
    let s = &state.settings;
    let db = &state.db;
    let str_of = |v: &serde_json::Value| v.as_str().unwrap_or("").to_string();
    let bool_of = |v: &serde_json::Value| v.as_bool().unwrap_or(false);
    let int_of = |v: &serde_json::Value| v.as_i64().unwrap_or(0);
    OidcConfig {
        enabled: bool_of(&s.get_setting(db, "oidc_enabled").await),
        issuer: str_of(&s.get_setting(db, "oidc_issuer").await),
        client_id: str_of(&s.get_setting(db, "oidc_client_id").await),
        client_secret: str_of(&s.get_setting(db, "oidc_client_secret").await),
        redirect_uri: str_of(&s.get_setting(db, "oidc_redirect_uri").await),
        scopes: str_of(&s.get_setting(db, "oidc_scopes").await),
        allow_signup: bool_of(&s.get_setting(db, "oidc_allow_signup").await),
        state_ttl: int_of(&s.get_setting(db, "oidc_state_ttl_seconds").await),
        admin_groups: str_of(&s.get_setting(db, "oidc_admin_groups").await),
        user_groups: str_of(&s.get_setting(db, "oidc_user_groups").await),
        require_signature: bool_of(&s.get_setting(db, "oidc_require_signature").await),
    }
}

/// The configured OIDC issuer base URL, trimmed and without a trailing slash
/// (empty when not configured).
fn oidc_effective_issuer(cfg: &OidcConfig) -> String {
    cfg.issuer.trim().trim_end_matches('/').to_string()
}

/// Resolve the public host for a request, in priority order:
/// `X-Forwarded-Host` (set by a reverse proxy), then the URI authority
/// (the HTTP/2 `:authority` pseudo-header, which hyper stores in the URI
/// rather than as a `Host` header), then the `Host` header (HTTP/1.1).
/// Reading only the `Host` header returns empty under HTTP/2, which is why
/// derived redirect URLs were coming out as `https:///...`.
pub fn public_host(headers: &HeaderMap, uri: &Uri) -> String {
    if let Some(v) = headers.get("x-forwarded-host").and_then(|v| v.to_str().ok()) {
        return v.to_string();
    }
    if let Some(authority) = uri.authority() {
        return authority.as_str().to_string();
    }
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Derive the public base URL for a request (scheme + host), honoring
/// reverse-proxy headers (`X-Forwarded-Proto` / `Host`) when present.
fn oidc_base_url(headers: &HeaderMap, uri: &Uri) -> String {
    let scheme = request_scheme(headers);
    let host = public_host(headers, uri);
    format!("{scheme}://{host}")
}

/// The redirect_uri registered with the provider. Prefer the explicit setting;
/// otherwise derive it from the request's public base URL.
fn oidc_redirect_uri(cfg: &OidcConfig, headers: &HeaderMap, uri: &Uri) -> String {
    let configured = cfg.redirect_uri.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    format!("{}/api/auth/oidc/callback", oidc_base_url(headers, uri))
}

/// The OIDC discovery-document cache (issuer -> document), mirroring
/// `api.OIDC_DISCOVERY_CACHE`. Public so tests can clear it between runs.
pub fn oidc_discovery_cache() -> &'static Mutex<HashMap<String, serde_json::Value>> {
    static CACHE: OnceLock<Mutex<HashMap<String, serde_json::Value>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fetch (and cache) the OIDC discovery document for the given issuer. Returns
/// None when discovery is unavailable (the caller maps that to a 502). Public
/// so the admin SSO "test connection" endpoint (and tests) can reuse the same
/// discovery path as the auth flow (mirrors the Python `_oidc_discovery`).
pub async fn oidc_discovery(issuer: &str) -> Option<serde_json::Value> {
    if let Some(doc) = oidc_discovery_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(issuer)
    {
        return Some(doc.clone());
    }
    let url = format!("{issuer}/.well-known/openid-configuration");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let r = client.get(&url).send().await.ok()?;
    if r.status() != 200 {
        warn!("OIDC discovery returned {} for {url}", r.status());
        return None;
    }
    let doc: serde_json::Value = r.json().await.ok()?;
    for field in ["issuer", "authorization_endpoint", "token_endpoint"] {
        if doc.get(field).is_none() {
            warn!("OIDC discovery document missing '{field}'.");
            return None;
        }
    }
    oidc_discovery_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(issuer.to_string(), doc.clone());
    Some(doc)
}

/// Parse the `groups` claim from an id_token into a list of group names. The
/// claim may be a comma-separated string or a JSON array.
fn oidc_groups_from_claims(claims: &serde_json::Value) -> Vec<String> {
    let groups = match claims.get("groups") {
        Some(g) => g,
        None => return Vec::new(),
    };
    if let Some(s) = groups.as_str() {
        return s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }
    if let Some(arr) = groups.as_array() {
        return arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }
    Vec::new()
}

/// Derive a sanitized Vreckan username from OIDC claims, preferring
/// `preferred_username`, then the email local-part, then `sub`.
fn oidc_username_from_claims(claims: &serde_json::Value) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(pu) = claims.get("preferred_username").and_then(|v| v.as_str()) {
        if !pu.is_empty() {
            candidates.push(pu.to_string());
        }
    }
    if let Some(email) = claims.get("email").and_then(|v| v.as_str()) {
        if let Some(local) = email.split_once('@').map(|(l, _)| l) {
            if !local.is_empty() {
                candidates.push(local.to_string());
            }
        }
    }
    if let Some(sub) = claims.get("sub").and_then(|v| v.as_str()) {
        if !sub.is_empty() {
            candidates.push(sub.to_string());
        }
    }
    for cand in candidates {
        let cleaned: String = cand
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let cleaned = cleaned.trim_matches('_');
        if !cleaned.is_empty() {
            return Some(cleaned.to_string());
        }
    }
    None
}

/// The configured OIDC admin group names as a set.
fn oidc_admin_groups_set(cfg: &OidcConfig) -> std::collections::HashSet<String> {
    cfg.admin_groups
        .replace(',', " ")
        .split_whitespace()
        .map(|t| t.to_string())
        .collect()
}

/// The configured OIDC user group names as a set.
fn oidc_user_groups_set(cfg: &OidcConfig) -> std::collections::HashSet<String> {
    cfg.user_groups
        .replace(',', " ")
        .split_whitespace()
        .map(|t| t.to_string())
        .collect()
}

/// Map provider (e.g. authentik) groups onto the Vreckan account. Under the
/// RBAC model this works by *group membership*: the full provider-group list is
/// recorded on the account, the user is added to a Vreckan group for every
/// provider group they're in (auto-creating the group if needed), and only the
/// groups named in the configured admin/user lists get roles auto-assigned on
/// login (the built-in `user` role for all of them, plus the `admin` role for
/// admin-list groups). Any other (newly observed) SSO group is created empty
/// and left for an admin to grant access to explicitly. One of the user's
/// groups is also set as their *primary* group for quota/settings overrides.
/// No-op if the id_token carries no recognizable `groups` claim. Public so the
/// admin SSO tests can drive it directly (mirrors the Python `_oidc_apply_groups`).
pub async fn oidc_apply_groups(
    state: &AppState,
    cfg: &OidcConfig,
    username: &str,
    claims: &serde_json::Value,
) {
    let user_groups = oidc_groups_from_claims(claims);
    if user_groups.is_empty() {
        return;
    }
    let conn = &state.db.conn;
    // Persist the full SSO group list on the account (idempotent).
    let _ = user_manager::set_sso_groups(&state.db, username, &user_groups).await;
    let admin_groups = oidc_admin_groups_set(cfg);
    let user_groups_set = oidc_user_groups_set(cfg);
    let user = match user_manager::get_user(&state.db, username).await {
        Ok(Some(u)) => u,
        _ => return,
    };
    // Join the user to a Vreckan group for every provider group they're in.
    let mut joined = user.groups.clone();
    for g in &user_groups {
        let _ = user_manager::ensure_group(&state.db, g).await;
        if admin_groups.contains(g) || user_groups_set.contains(g) {
            if let Ok(Some(group)) = get_group_row(conn, g).await {
                let mut roles = group.roles.clone();
                if !roles.contains(&"user".to_string()) {
                    roles.push("user".to_string());
                }
                if admin_groups.contains(g) && !roles.contains(&"admin".to_string()) {
                    roles.push("admin".to_string());
                }
                let _ = db_save_group(conn, g, &group.settings, Some(&roles), None).await;
            }
        }
        if !joined.contains(g) {
            joined.push(g.clone());
        }
    }
    let _ = db_save_user_row(
        conn,
        username,
        &user_manager::UserFields {
            groups: Some(joined),
            ..Default::default()
        },
    )
    .await;
    // Pick a primary group (for quota/settings overrides): prefer a group in
    // the configured user-groups list, else the first non-admin group, else the
    // first group.
    let primary = user_groups
        .iter()
        .find(|g| user_groups_set.contains(*g))
        .or_else(|| user_groups.iter().find(|g| !admin_groups.contains(*g)))
        .cloned()
        .unwrap_or_else(|| user_groups.first().cloned().unwrap_or_default());
    let mut settings_dict = user
        .settings
        .clone()
        .unwrap_or_else(|| user_manager::default_user_settings().clone());
    settings_dict["group"] = serde_json::Value::String(primary);
    let _ = db_save_user_row(
        conn,
        username,
        &user_manager::UserFields {
            settings: Some(settings_dict),
            ..Default::default()
        },
    )
    .await;
    // Re-derive is_admin from the (now group-driven) effective permissions.
    let _ = user_manager::recompute_is_admin(&state.db, username).await;
}

/// `jwks 0.5.x` still compiles against `jsonwebtoken` v10, so the
/// `DecodingKey` stored on its `Jwk` is a v10 type. vreckan decodes with v11,
/// so translate the v10 key into the equivalent v11 key before use (the
/// `jsonwebtoken_v10` companion dep in Cargo.toml names that v10 crate).
fn v11_decoding_key(
    v10_key: &jsonwebtoken_v10::DecodingKey,
) -> Option<jsonwebtoken::DecodingKey> {
    use jsonwebtoken_v10::DecodingKeyKind;
    match v10_key.kind() {
        DecodingKeyKind::RsaModulusExponent { n, e } => {
            Some(jsonwebtoken::DecodingKey::from_rsa_raw_components(n, e))
        }
        DecodingKeyKind::SecretOrDer(der) => match v10_key.family() {
            jsonwebtoken_v10::AlgorithmFamily::Rsa => {
                Some(jsonwebtoken::DecodingKey::from_rsa_der(der))
            }
            jsonwebtoken_v10::AlgorithmFamily::Ec => {
                Some(jsonwebtoken::DecodingKey::from_ec_der(der))
            }
            // HMAC/Ed keys can never satisfy the RS256 validation below;
            // treat as "no usable key" (fail-closed path).
            _ => None,
        },
    }
}

/// Verify the id_token signature (via the provider's JWKS) and standard claims
/// (aud/exp), then return its claims. The signature algorithm is pinned to
/// RS256 (the de-facto OIDC standard) rather than trusted from the token
/// header, which defeats the algorithm-confusion class of attacks. When the
/// provider's JWKS is unreachable the behaviour is governed by the
/// `oidc_require_signature` setting: by default the login fails closed (a
/// signature is mandatory); setting it to false restores the legacy
/// claims-only fallback for providers that omit a JWKS.
async fn oidc_verify_id_token(
    cfg: &OidcConfig,
    id_token: &str,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let header = jsonwebtoken::decode_header(id_token)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Invalid id_token header: {e}")))?;
    let kid = header.kid.clone();

    // Resolve the provider's JWKS (discovery is normally already cached from
    // the authorize step; if not, fetch it now).
    let issuer = oidc_effective_issuer(cfg);
    let mut discovery = oidc_discovery_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&issuer)
        .cloned();
    if discovery.is_none() {
        discovery = oidc_discovery(&issuer).await;
    }
    let jwks_uri = discovery
        .as_ref()
        .and_then(|d| d.get("jwks_uri"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let decoding_key = if let Some(jwks_uri) = jwks_uri {
        match Jwks::from_jwks_url(&jwks_uri).await {
            Ok(jwks) => {
                // Match by kid when present; otherwise take the first key.
                let entry = if let Some(k) = &kid {
                    jwks.keys.get(k)
                } else {
                    jwks.keys.values().next()
                };
                entry.and_then(|j| v11_decoding_key(&j.decoding_key))
            }
            Err(e) => {
                warn!("OIDC JWKS fetch/parse failed: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_audience(&[cfg.client_id.as_str()]);
    // `iss` is checked manually below (to tolerate a trailing slash).
    validation.validate_exp = true;

    if let Some(key) = decoding_key {
        let claims = jsonwebtoken::decode::<serde_json::Value>(id_token, &key, &validation)
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("id_token verification failed: {e}")))?
            .claims;
        // Tolerate a trailing slash: some providers sign `iss` with a trailing
        // "/" while we store the issuer without one.
        let iss = claims.get("iss").and_then(|v| v.as_str()).unwrap_or("");
        if iss.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err((StatusCode::BAD_GATEWAY, "id_token issuer mismatch.".to_string()));
        }
        return Ok(claims);
    }

    // No JWKS was available, so no signature could be checked.
    if cfg.require_signature {
        warn!("OIDC id_token: no JWKS available; failing closed.");
        return Err((
            StatusCode::BAD_GATEWAY,
            "OIDC provider JWKS unavailable; could not verify the id_token signature.".to_string(),
        ));
    }
    // Legacy behaviour: verify claims only and warn loudly.
    warn!("OIDC id_token: no JWKS available; verifying claims only.");
    let claims = jsonwebtoken::dangerous::insecure_decode::<serde_json::Value>(id_token)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("id_token verification failed: {e}")))?
        .claims;
    let iss = claims.get("iss").and_then(|v| v.as_str()).unwrap_or("");
    if iss.trim_end_matches('/') != issuer.trim_end_matches('/') {
        return Err((StatusCode::BAD_GATEWAY, "id_token issuer mismatch.".to_string()));
    }
    Ok(claims)
}

/// `GET /api/auth/oidc/status` — report whether OIDC/SSO is enabled and fully
/// configured. Unauthenticated (the browser needs this on the login screen
/// before any session exists).
async fn oidc_status(State(state): State<AppState>) -> Response {
    let cfg = oidc_config(&state).await;
    let issuer = oidc_effective_issuer(&cfg);
    let enabled = cfg.enabled && !issuer.is_empty() && !cfg.client_id.is_empty();
    Json(serde_json::json!({ "enabled": enabled, "configured": enabled }))
        .into_response()
}

/// `GET /api/auth/oidc/authorize` — begin the OIDC authorization-code flow.
/// Generates a one-time `state`, stores it, and 302-redirects the browser to
/// the provider.
async fn oidc_authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
) -> Response {
    let cfg = oidc_config(&state).await;
    let issuer = oidc_effective_issuer(&cfg);
    if !(cfg.enabled && !issuer.is_empty() && !cfg.client_id.is_empty()) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "OIDC/SSO is not enabled on this server.",
        );
    }
    let state_str = generate_token();
    let now = now_secs();
    let ttl = if cfg.state_ttl > 0 { cfg.state_ttl } else { 600 };
    {
        let mut states = state.oidc_states.write().await;
        // Prune expired states while we hold the lock.
        let expired: Vec<String> = states
            .iter()
            .filter(|(_, v)| v.expires_at < now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            states.remove(&k);
        }
        states.insert(
            state_str.clone(),
            crate::state::OidcStateRecord {
                expires_at: now + ttl as f64,
            },
        );
    }
    let discovery = match oidc_discovery(&issuer).await {
        Some(d) => d,
        None => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "Unable to reach the OIDC provider (discovery).",
            )
        }
    };
    let auth_endpoint = discovery
        .get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut url = match url::Url::parse(auth_endpoint) {
        Ok(u) => u,
        Err(e) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("Invalid OIDC authorization endpoint: {e}"),
            )
        }
    };
    // NOTE: no `prompt=login` here. With an authorization-code flow, sending
    // prompt=login makes the IdP force a *second* full re-authentication after
    // the first one, which is bad UX and can push the round-trip past the state
    // TTL so the callback fails the CSRF check.
    let params = vec![
        ("response_type".to_string(), "code".to_string()),
        ("client_id".to_string(), cfg.client_id.clone()),
        (
            "redirect_uri".to_string(),
            oidc_redirect_uri(&cfg, &headers, &uri),
        ),
        (
            "scope".to_string(),
            if cfg.scopes.is_empty() {
                "openid email profile".to_string()
            } else {
                cfg.scopes.clone()
            },
        ),
        ("state".to_string(), state_str),
    ];
    url.query_pairs_mut().clear().extend_pairs(params);
    (
        [(
            header::LOCATION,
            url.as_str().to_string(),
        )],
        StatusCode::FOUND,
    )
        .into_response()
}

/// Query params for `GET /api/auth/oidc/callback`.
#[derive(Deserialize)]
struct OidcCallbackQuery {
    code: String,
    state: String,
}

/// `GET /api/auth/oidc/callback` — OIDC redirect target. Validates `state`,
/// exchanges the code for tokens, verifies the id_token, provisions/logs in
/// the user, sets the web token cookie, and redirects to the app root.
async fn oidc_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    Query(q): Query<OidcCallbackQuery>,
) -> Response {
    // 1) Validate and consume the one-time state.
    let now = now_secs();
    let st = state.oidc_states.write().await.remove(&q.state);
    if !st.is_some_and(|s| s.expires_at >= now) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Invalid or expired OIDC state (CSRF check failed).",
        );
    }
    let cfg = oidc_config(&state).await;
    let issuer = oidc_effective_issuer(&cfg);
    let redirect_uri = oidc_redirect_uri(&cfg, &headers, &uri);

    // 2) Exchange the authorization code for tokens.
    let discovery = match oidc_discovery(&issuer).await {
        Some(d) => d,
        None => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "Unable to reach the OIDC provider (discovery).",
            )
        }
    };
    let token_endpoint = discovery
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut data: Vec<(&str, String)> = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", q.code.clone()),
        ("redirect_uri", redirect_uri.clone()),
        ("client_id", cfg.client_id.clone()),
    ];
    if !cfg.client_secret.is_empty() {
        data.push(("client_secret", cfg.client_secret.clone()));
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to build HTTP client: {e}"),
            )
        }
    };
    let tr = match client.post(token_endpoint).form(&data).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("OIDC token exchange failed: {e}");
            return json_error(StatusCode::BAD_GATEWAY, "OIDC token exchange failed.");
        }
    };
    let status = tr.status();
    let body = tr.bytes().await.unwrap_or_default();
    if status != 200 {
        let snippet = String::from_utf8_lossy(&body[..body.len().min(200)]);
        warn!("OIDC token endpoint returned {status}: {snippet}");
        return json_error(StatusCode::BAD_GATEWAY, "OIDC token exchange failed.");
    }
    let tokens: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(t) => t,
        Err(_) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "OIDC token endpoint returned invalid JSON.",
            )
        }
    };
    let id_token = match tokens.get("id_token").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "OIDC provider did not return an id_token.",
            )
        }
    };

    // 3) Verify the id_token (signature + claims).
    let claims = match oidc_verify_id_token(&cfg, &id_token).await {
        Ok(c) => c,
        Err((status, detail)) => return json_error(status, &detail),
    };

    // 4) Derive a username; if the user does not exist, auto-provision.
    let username = match oidc_username_from_claims(&claims) {
        Some(u) => u,
        None => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "Could not derive a username from the OIDC identity.",
            )
        }
    };
    // 4) If the user does not exist, auto-provision (when allowed).
    match user_manager::get_user(&state.db, &username).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            if !cfg.allow_signup {
                return json_error(
                    StatusCode::FORBIDDEN,
                    "Unknown user and OIDC auto-signup is disabled.",
                );
            }
            if let Err(e) = user_manager::create_user(&state.db, &username, None).await {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    &format!("Could not provision user: {e}"),
                );
            }
        }
        Err(e) => return user_error_response(e),
    };
    // 4a) Flag the account as SSO-provisioned (idempotent).
    let _ = user_manager::mark_user_sso(&state.db, &username).await;
    // 4b) Map provider groups onto the account (mutates the on-disk user).
    let _ = oidc_apply_groups(&state, &cfg, &username, &claims).await;
    // No admin bypass: resolve the effective setting so a disabled admin is
    // rejected at the SSO callback exactly like any other inactive account.
    let effective = match user_manager::get_effective_settings(&state.db, &username).await {
        Ok(e) => e,
        Err(e) => return user_error_response(e),
    };
    if !effective.get("active").and_then(|v| v.as_bool()).unwrap_or(false) {
        return json_error(StatusCode::FORBIDDEN, "User account is inactive.");
    }
    // 5) Issue the web token cookie (same mechanism as password login).
    let token = match issue_auth_token(&state, &username).await {
        Ok(t) => t,
        Err(e) => return user_error_response(e),
    };
    let _ = save_auth_tokens(&state).await;
    // 6) Redirect to the app root.
    let secure = request_scheme(&headers) == "https";
    let cookie = auth_cookie(&state.settings, Some(&token), secure);
    (
        [
            (header::LOCATION, "/".to_string()),
            (header::SET_COOKIE, cookie),
        ],
        StatusCode::FOUND,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    fn settings_with_proxies(list: &str) -> Settings {
        let mut s = Settings::from_env();
        s.trusted_proxies = list.to_string();
        s
    }

    fn xff(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_static("x-forwarded-for"),
            value.parse().unwrap(),
        );
        h
    }

    #[test]
    fn xff_ignored_when_no_trusted_proxy_configured() {
        let s = settings_with_proxies("");
        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 1234);
        assert_eq!(client_ip(&s, &xff("9.9.9.9"), Some(remote)), "1.2.3.4");
    }

    #[test]
    fn xff_honoured_only_when_direct_peer_is_trusted() {
        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), 1234);
        // Exact-IP entry matching the direct peer.
        let s = settings_with_proxies("10.1.2.3");
        assert_eq!(client_ip(&s, &xff("9.9.9.9"), Some(remote)), "9.9.9.9");
        // CIDR covering the direct peer; leftmost entry wins.
        let s = settings_with_proxies("10.0.0.0/8");
        assert_eq!(
            client_ip(&s, &xff("9.9.9.9, 10.1.2.3"), Some(remote)),
            "9.9.9.9"
        );
        // A different, untrusted peer: XFF falls back to the direct address.
        let other = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 1234);
        assert_eq!(client_ip(&s, &xff("9.9.9.9"), Some(other)), "203.0.113.7");
    }

    #[test]
    fn peer_is_trusted_proxy_match_rules() {
        // Empty list never matches.
        let s = settings_with_proxies("");
        let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 1);
        assert!(!peer_is_trusted_proxy(&s, Some(v4)));
        // No direct peer (test harnesses) never matches.
        assert!(!peer_is_trusted_proxy(&s, None));
        // Bare IP, whitespace-tolerant list, CIDR.
        let s = settings_with_proxies(" 10.0.0.1 , 192.168.1.0/24 ");
        assert!(peer_is_trusted_proxy(&s, Some(v4)));
        let v4b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 55)), 1);
        assert!(peer_is_trusted_proxy(&s, Some(v4b)));
        let v4c = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 55)), 1);
        assert!(!peer_is_trusted_proxy(&s, Some(v4c)));
        // IPv6: exact and /128 CIDR.
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 1);
        let s6 = settings_with_proxies("::1/128, ::1");
        assert!(peer_is_trusted_proxy(&s6, Some(v6)));
        // Version mismatch never matches.
        assert!(!peer_is_trusted_proxy(&settings_with_proxies("127.0.0.1/32"), Some(v6)));
        // /0 matches every v4 peer.
        let s0 = settings_with_proxies("0.0.0.0/0");
        assert!(peer_is_trusted_proxy(&s0, Some(v4c)));
        // Malformed entries never match.
        let s_bad = settings_with_proxies("10.0.0.0/129, 10.0.0.0.0/8, notanip");
        assert!(!peer_is_trusted_proxy(&s_bad, Some(v4)));
    }

    #[test]
    fn xff_with_empty_leading_entry_falls_back() {
        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), 1234);
        let s = settings_with_proxies("10.1.2.3");
        assert_eq!(client_ip(&s, &xff(", 9.9.9.9"), Some(remote)), "10.1.2.3");
        // No remote at all: shared unknown bucket.
        assert_eq!(client_ip(&s, &xff("9.9.9.9"), None), "unknown");
    }
}
