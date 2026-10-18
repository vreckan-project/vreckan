//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Share-session links (ANT-305): let an owner share a running app session
//! with anyone via a link, with per-link capability toggles, an optional
//! password, an optional expiry, and per-link revocation.
//!
//! Mirrors the public file-share model in `files.rs` (`public_shares`): a
//! durable `session_shares` table, an owner-only E2EE management API under
//! `/api/sessions/{id}/shares`, and a plaintext, account-less entry point at
//! `GET/POST /share/{share_id}`.
//!
//! Each visit to a share mints a fresh per-visit token. The visitor is
//! recorded in the session's `viewers` list (so the proxy and, for rooms, the
//! room UI know about them) and in the container's selkies token table. Plain
//! (non-collaboration) sessions are promoted to selkies "secure mode"
//! lazily on their first share visit: a master token is minted and the full
//! token table is pushed. When the container's `/api/tokens` endpoint is
//! absent (older selkies images without the control plane) the session stays
//! legacy and the visitor's role rides the `?role=` query parameter instead —
//! the redirect URL always carries both `?token=` and `?role=` so one URL
//! works on either kind of container.

use std::collections::HashMap;

use axum::extract::{Path as PathParam, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use rand::RngExt;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::auth;
use crate::catalog;
use crate::collaboration;
use crate::e2ee;
use crate::entities;
use crate::files;
use crate::state::AppState;
use crate::user_manager;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The per-link capability toggles for a session share. Enforced server-side
/// in the session proxy (WP3); the room UI reflects them for room sessions.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionShareCaps {
    pub input: bool,
    pub file_browse: bool,
    pub file_download: bool,
    pub file_upload: bool,
}

/// The durable record for one share-session link (stored as the
/// `session_shares.data` JSON payload, mirroring `PublicShareMetadata`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionShareMetadata {
    pub session_id: String,
    pub owner_username: String,
    /// "full" (input + all file caps) or "view_only" (no input; browse and
    /// download on by default).
    pub role: String,
    pub caps: SessionShareCaps,
    pub created_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_timestamp: Option<f64>,
    /// True once the owner revoked the link (a revoked link keeps its row so
    /// the public entry can answer 410 and the owner's list can show it).
    #[serde(default)]
    pub revoked: bool,
}

/// A per-visit token for a session share (in-memory only, mirroring the
/// `download_tokens` pattern). The session proxy (WP3) looks a visitor's
/// token up here to find the share that granted it and enforces the
/// capability set against the in-container file API.
///
/// `caps` is a *snapshot* of the share's caps taken when the visit was
/// minted: a mid-visit edit to the share's caps therefore applies on the
/// visitor's next open, not live (the cheap semantics chosen in ANT-306).
/// Revocation is the exception — it removes the record, so a revoked link
/// cuts its visits immediately.
#[derive(Clone, Debug)]
pub struct SessionShareTokenRecord {
    pub share_id: String,
    pub session_id: String,
    pub caps: SessionShareCaps,
}

/// The wire representation of a share for the owner management API.
#[derive(Serialize)]
pub struct SessionShareInfo {
    pub share_id: String,
    pub session_id: String,
    pub role: String,
    pub caps: SessionShareCaps,
    pub created_at: f64,
    pub expiry_timestamp: Option<f64>,
    pub has_password: bool,
    pub revoked: bool,
    pub url: String,
}

#[derive(Deserialize)]
pub struct CreateSessionShareRequest {
    pub role: String,
    #[serde(default)]
    pub caps: Option<PartialCaps>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub expiry_hours: Option<i64>,
}

/// A present `null` must be told apart from an absent key (the outer
/// `Option` would otherwise collapse both to `None`). Only invoked when the
/// key is present; absence is handled by `#[serde(default)]`.
fn de_present_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::<String>::deserialize(d)?))
}

fn de_present_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<i64>>, D::Error> {
    Ok(Some(Option::<i64>::deserialize(d)?))
}

#[derive(Deserialize)]
pub struct UpdateSessionShareRequest {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub caps: Option<PartialCaps>,
    /// `null` clears the password; `""` does too; a non-empty string sets it.
    #[serde(default, deserialize_with = "de_present_string")]
    pub password: Option<Option<String>>,
    /// `null` clears the expiry; a positive number sets a new one.
    #[serde(default, deserialize_with = "de_present_i64")]
    pub expiry_hours: Option<Option<i64>>,
}

/// A partial capability override for create/update. Absent fields fall back
/// to the role's defaults (full: all on; view_only: browse + download).
#[derive(Deserialize, Default)]
pub struct PartialCaps {
    #[serde(default)]
    pub input: Option<bool>,
    #[serde(default)]
    pub file_browse: Option<bool>,
    #[serde(default)]
    pub file_download: Option<bool>,
    #[serde(default)]
    pub file_upload: Option<bool>,
}

#[derive(Deserialize)]
pub struct SessionSharePasswordForm {
    pub password: String,
}

// ---------------------------------------------------------------------------
// Durable store (mirrors the public-share load/save in `files.rs`)
// ---------------------------------------------------------------------------

/// Load session-share metadata from the database into `state.session_shares`.
pub async fn load_session_shares(state: &AppState) {
    let rows = match entities::session_share::Entity::find().all(&state.db.conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Error loading session shares from the database: {e}");
            return;
        }
    };
    let mut shares = HashMap::new();
    for row in rows {
        match serde_json::from_value::<SessionShareMetadata>(row.data) {
            Ok(meta) => {
                shares.insert(row.share_id, meta);
            }
            Err(e) => {
                warn!("Skipping malformed session share row: {e}");
            }
        }
    }
    let count = shares.len();
    *state.session_shares.write().await = shares;
    info!("Loaded {count} session share(s) from the database.");
}

/// Persist the in-memory session-share map to the database (full replace).
pub async fn save_session_shares(state: &AppState) {
    let shares = state.session_shares.read().await.clone();
    let conn = &state.db.conn;
    let all = match entities::session_share::Entity::find().all(conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to save session shares (delete): {e}");
            return;
        }
    };
    for row in all {
        let am = entities::session_share::ActiveModel {
            share_id: sea_orm::ActiveValue::Set(row.share_id),
            data: sea_orm::ActiveValue::NotSet,
        };
        if let Err(e) = entities::session_share::Entity::delete(am).exec(conn).await {
            warn!("Failed to delete session share row: {e}");
        }
    }
    for (share_id, meta) in shares.iter() {
        let data = match serde_json::to_value(meta) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to serialize session share: {e}");
                continue;
            }
        };
        let am = entities::session_share::ActiveModel {
            share_id: sea_orm::ActiveValue::Set(share_id.clone()),
            data: sea_orm::ActiveValue::Set(data),
        };
        if let Err(e) = entities::session_share::Entity::insert(am).exec(conn).await {
            warn!("Failed to insert session share row: {e}");
        }
    }
    info!("Saved {} session share(s) to the database.", shares.len());
}

fn now_unix_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Capability invariants
// ---------------------------------------------------------------------------

/// The default capability set for a share role (the "standard view-only"
/// preset is browse + download; a full share enables everything).
fn default_caps_for_role(role: &str) -> SessionShareCaps {
    if role == "full" {
        SessionShareCaps {
            input: true,
            file_browse: true,
            file_download: true,
            file_upload: true,
        }
    } else {
        SessionShareCaps {
            input: false,
            file_browse: true,
            file_download: true,
            file_upload: false,
        }
    }
}

/// Apply the role invariants on top of a capability base: a `full` share
/// always has every cap on; a `view_only` share never has input.
fn resolve_caps(role: &str, base: &SessionShareCaps, patch: Option<&PartialCaps>) -> SessionShareCaps {
    let mut caps = base.clone();
    if let Some(p) = patch {
        if let Some(v) = p.input {
            caps.input = v;
        }
        if let Some(v) = p.file_browse {
            caps.file_browse = v;
        }
        if let Some(v) = p.file_download {
            caps.file_download = v;
        }
        if let Some(v) = p.file_upload {
            caps.file_upload = v;
        }
    }
    if role == "full" {
        caps = SessionShareCaps {
            input: true,
            file_browse: true,
            file_download: true,
            file_upload: true,
        };
    } else if role == "view_only" {
        caps.input = false;
    }
    caps
}

// ---------------------------------------------------------------------------
// Owner management API (E2EE, own session only)
// ---------------------------------------------------------------------------

/// `POST /api/sessions/{session_id}/shares` — create a share for the session.
async fn create_session_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(session_id): PathParam<String>,
    body: axum::body::Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                axum::Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let session = state.sessions.read().await.get(&session_id).cloned();
    let Some(session) = session else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found." }),
        )
        .await;
    };
    if session.get("username").and_then(|v| v.as_str()) != Some(user.record.username.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Not your session." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d }))
                .await
        }
    };
    let req: CreateSessionShareRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    if req.role != "full" && req.role != "view_only" {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "role must be 'full' or 'view_only'." }),
        )
        .await;
    }
    let caps = resolve_caps(&req.role, &default_caps_for_role(&req.role), req.caps.as_ref());
    let password_hash = req
        .password
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(user_manager::hash_share_password);
    let now = now_unix_secs();
    let expiry_timestamp = req
        .expiry_hours
        .filter(|h| *h > 0)
        .map(|h| now + (h as f64) * 3600.0);
    let share_id = catalog::random_uuid();
    let meta = SessionShareMetadata {
        session_id: session_id.clone(),
        owner_username: user.record.username.clone(),
        role: req.role,
        caps,
        created_at: now,
        password_hash,
        expiry_timestamp,
        revoked: false,
    };
    {
        let mut shares = state.session_shares.write().await;
        shares.insert(share_id.clone(), meta.clone());
    }
    save_session_shares(&state).await;
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        share_info_json(&share_id, &meta),
    )
    .await
}

/// `GET /api/sessions/{session_id}/shares` — list the session's shares.
async fn list_session_shares(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(session_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                axum::Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let session = state.sessions.read().await.get(&session_id).cloned();
    let Some(session) = session else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found." }),
        )
        .await;
    };
    if session.get("username").and_then(|v| v.as_str()) != Some(user.record.username.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Not your session." }),
        )
        .await;
    }
    let shares = state.session_shares.read().await;
    let mut out: Vec<Value> = shares
        .iter()
        .filter(|(_, meta)| meta.session_id == session_id)
        .map(|(sid, meta)| share_info_json(sid, meta))
        .collect();
    out.sort_by(|a, b| {
        b.get("created_at")
            .and_then(|v| v.as_f64())
            .partial_cmp(&a.get("created_at").and_then(|v| v.as_f64()))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, Value::Array(out)).await
}

/// `PATCH /api/sessions/{session_id}/shares/{share_id}` — update a share.
async fn update_session_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam((session_id, share_id)): PathParam<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                axum::Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let session = state.sessions.read().await.get(&session_id).cloned();
    let Some(session) = session else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found." }),
        )
        .await;
    };
    if session.get("username").and_then(|v| v.as_str()) != Some(user.record.username.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Not your session." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d }))
                .await
        }
    };
    let req: UpdateSessionShareRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    let mut shares = state.session_shares.write().await;
    let Some(meta) = shares.get_mut(&share_id) else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Share not found." }),
        )
        .await;
    };
    if meta.session_id != session_id {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Share not found." }),
        )
        .await;
    }
    if let Some(role) = req.role.as_deref() {
        if role != "full" && role != "view_only" {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": "role must be 'full' or 'view_only'." }),
            )
            .await;
        }
        meta.role = role.to_string();
    }
    // A role change applies the invariants on top of the current caps (a
    // switch to `full` turns every cap on; to `view_only` it only removes
    // input, leaving the file caps as configured).
    meta.caps = resolve_caps(&meta.role, &meta.caps, req.caps.as_ref());
    if let Some(pw) = req.password {
        meta.password_hash = pw
            .filter(|p| !p.is_empty())
            .as_deref()
            .map(user_manager::hash_share_password);
    }
    if let Some(hours) = req.expiry_hours {
        meta.expiry_timestamp = hours
            .filter(|h| *h > 0)
            .map(|h| now_unix_secs() + (h as f64) * 3600.0);
    }
    let updated = meta.clone();
    // Drop the write guard before the save: the save re-reads the map and
    // tokio locks are not reentrant.
    drop(shares);
    save_session_shares(&state).await;
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        share_info_json(&share_id, &updated),
    )
    .await
}

/// `DELETE /api/sessions/{session_id}/shares/{share_id}` — revoke a share.
async fn revoke_session_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam((session_id, share_id)): PathParam<(String, String)>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                axum::Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let session = state.sessions.read().await.get(&session_id).cloned();
    let Some(session) = session else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found." }),
        )
        .await;
    };
    if session.get("username").and_then(|v| v.as_str()) != Some(user.record.username.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Not your session." }),
        )
        .await;
    }
    let shares = state.session_shares.read().await;
    let Some(meta) = shares.get(&share_id) else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Share not found." }),
        )
        .await;
    };
    if meta.session_id != session_id {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Share not found." }),
        )
        .await;
    }
    drop(shares);
    // Revoking strips every per-visit token this share has ever minted (from
    // the session record, the container's token table, and the vreckan-side
    // token map) and marks the row revoked so the public entry answers 410.
    strip_share_tokens(&state, &share_id).await;
    {
        let mut shares = state.session_shares.write().await;
        if let Some(meta) = shares.get_mut(&share_id) {
            meta.revoked = true;
        }
    }
    save_session_shares(&state).await;
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, Value::Null).await
}

fn share_info_json(share_id: &str, meta: &SessionShareMetadata) -> Value {
    serde_json::json!({
        "share_id": share_id,
        "session_id": meta.session_id,
        "role": meta.role,
        "caps": meta.caps,
        "created_at": meta.created_at,
        "expiry_timestamp": meta.expiry_timestamp,
        "has_password": meta.password_hash.is_some(),
        "revoked": meta.revoked,
        "url": format!("/share/{share_id}"),
    })
}

// ---------------------------------------------------------------------------
// Public entry (plaintext, account-less): GET / share/{share_id}
// ---------------------------------------------------------------------------

/// Resolve the share for a public entry: `Ok(share)` when it exists,
/// `Err((status, message))` for the various dead-link states (404 unknown,
/// 410 revoked/expired/session-ended).
async fn resolve_share_for_visit(
    state: &AppState,
    share_id: &str,
) -> Result<SessionShareMetadata, (StatusCode, String)> {
    let shares = state.session_shares.read().await;
    let Some(meta) = shares.get(share_id).cloned() else {
        return Err((StatusCode::NOT_FOUND, "Share not found.".to_string()));
    };
    if meta.revoked {
        return Err((StatusCode::GONE, "This link has been revoked.".to_string()));
    }
    if let Some(exp) = meta.expiry_timestamp {
        if exp < now_unix_secs() {
            return Err((StatusCode::GONE, "This link has expired.".to_string()));
        }
    }
    let session = state.sessions.read().await.get(&meta.session_id).cloned();
    if session.is_none() {
        return Err((
            StatusCode::GONE,
            "The session this link points to has ended.".to_string(),
        ));
    }
    Ok(meta)
}

/// `GET /share/{share_id}` — the public entry: password page where
/// applicable, otherwise immediately grant access and redirect.
async fn access_session_share_get(
    State(state): State<AppState>,
    PathParam(share_id): PathParam<String>,
) -> Response {
    let meta = match resolve_share_for_visit(&state, &share_id).await {
        Ok(m) => m,
        Err((s, d)) => return (s, Html(format!("<h1>{d}</h1>"))).into_response(),
    };
    if meta.password_hash.is_some() {
        return files::render_password_page(&format!("/share/{share_id}"), &share_id, "", StatusCode::OK);
    }
    match grant_session_share_access(&state, &share_id, &meta).await {
        Some(redirect) => see_other(redirect),
        None => (
            StatusCode::GONE,
            Html("<h1>The session this link points to has ended.</h1>"),
        )
            .into_response(),
    }
}

/// `POST /share/{share_id}` — submit the share password.
async fn access_session_share_post(
    State(state): State<AppState>,
    PathParam(share_id): PathParam<String>,
    form: axum::extract::Form<SessionSharePasswordForm>,
) -> Response {
    let meta = match resolve_share_for_visit(&state, &share_id).await {
        Ok(m) => m,
        Err((s, d)) => return (s, Html(format!("<h1>{d}</h1>"))).into_response(),
    };
    let Some(hash) = &meta.password_hash else {
        return (
            StatusCode::BAD_REQUEST,
            "This share is not password protected.",
        )
            .into_response();
    };
    if user_manager::verify_share_password(hash, &form.password) {
        match grant_session_share_access(&state, &share_id, &meta).await {
            Some(redirect) => see_other(redirect),
            None => (
                StatusCode::GONE,
                Html("<h1>The session this link points to has ended.</h1>"),
            )
                .into_response(),
        }
    } else {
        files::render_password_page(
            &format!("/share/{share_id}"),
            &share_id,
            "Incorrect password. Please try again.",
            StatusCode::UNAUTHORIZED,
        )
    }
}

fn see_other(redirect: String) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, redirect)
        .body(axum::body::Body::empty())
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

// ---------------------------------------------------------------------------
// Granting access (per-visit tokens + lazy secure-mode promotion)
// ---------------------------------------------------------------------------

/// Grant one visit to a share: mint a per-visit token, record the visitor in
/// the session (viewers list + container token table), lazily promote a
/// plain session to selkies secure mode when supported, and return the
/// redirect URL that admits the visitor.
pub async fn grant_session_share_access(
    state: &AppState,
    share_id: &str,
    share: &SessionShareMetadata,
) -> Option<String> {
    let session_id = &share.session_id;
    let mut session = state.sessions.read().await.get(session_id).cloned()?;

    let is_collab = session
        .get("is_collaboration")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let visit_token = catalog::random_token(16);
    let permission = if share.caps.input {
        "participant"
    } else {
        "readonly"
    };

    // Record the per-visit token on the vreckan side (WP3 resolves a
    // visitor's caps through this map). The caps snapshot is what gives a
    // mid-visit cap edit its "next open" semantics (ANT-306).
    {
        let mut tokens = state.session_share_tokens.write().await;
        tokens.insert(
            visit_token.clone(),
            SessionShareTokenRecord {
                share_id: share_id.to_string(),
                session_id: session_id.clone(),
                caps: share.caps.clone(),
            },
        );
    }

    // Add the visitor to the session's viewers list. In a room session the
    // container role stays "viewer" (the room's MK/slot mechanism provides
    // input for participants); a full share on a plain session gets a
    // controller entry in the token table instead.
    let username = format!("User-{}", rand::rng().random_range(100..999));
    if let Some(viewers) = session.get_mut("viewers").and_then(|v| v.as_array_mut()) {
        viewers.push(serde_json::json!({
            "token": visit_token,
            "slot": Value::Null,
            "mk_control": false,
            "username": username,
            "permission": permission,
        }));
        if !is_collab && share.caps.input {
            if let Some(v) = viewers
                .iter_mut()
                .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(visit_token.as_str()))
            {
                v["role"] = Value::String("controller".to_string());
            }
        }
    }

    // Lazy secure-mode promotion: for a plain session, mint a master token
    // and push the full token table to the container. When the container has
    // no token control plane (older selkies images answer 404 / are
    // unreachable) the session stays legacy and the visitor's role rides the
    // `?role=` query parameter instead.
    let mut promoted = false;
    if !is_collab {
        let had_master = session
            .get("master_token")
            .and_then(|v| v.as_str())
            .map(|m| !m.is_empty())
            .unwrap_or(false);
        if !had_master {
            session["master_token"] = Value::String(catalog::random_token(16));
        }
        if collaboration::push_token_table(state, session_id, &session).await {
            promoted = true;
        } else if !had_master {
            // The push failed: this container isn't in secure mode. Undo the
            // mint (and the controller role, which only means something to a
            // secure-mode container).
            session.as_object_mut().map(|o| o.remove("master_token"));
            if let Some(viewers) = session.get_mut("viewers").and_then(|v| v.as_array_mut()) {
                if let Some(v) = viewers
                    .iter_mut()
                    .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(visit_token.as_str()))
                {
                    v.as_object_mut().map(|o| o.remove("role"));
                }
            }
        }
    }

    *state.sessions.write().await.entry(session_id.clone()).or_insert(Value::Object(
        serde_json::Map::new(),
    )) = session.clone();
    catalog::save_sessions(state).await;
    if is_collab {
        info!(
            "[{session_id}] Session share {share_id}: granted visitor ({permission})"
        );
    }

    if is_collab {
        Some(format!("/room/{session_id}?token={visit_token}"))
    } else if promoted {
        Some(format!("/api/apps/session/{session_id}/?token={visit_token}"))
    } else if share.caps.input {
        // Legacy container: the default role is controller.
        Some(format!("/api/apps/session/{session_id}/?token={visit_token}"))
    } else {
        Some(format!(
            "/api/apps/session/{session_id}/?token={visit_token}&role=viewer"
        ))
    }
}

/// Strip every per-visit token minted by the share with the given id: from
/// the session's viewers list, the container's token table (when one exists),
/// and the vreckan-side token map. Used by revocation and expiry cleanup.
pub async fn strip_share_tokens(state: &AppState, share_id: &str) {
    // Note: clone out of each lock in a plain statement. A lock guard
    // temporary held across an `if let` scrutinee survives until the end of
    // the block and would deadlock against the re-lock below (tokio locks
    // are not reentrant).
    let share_opt = state.session_shares.read().await.get(share_id).cloned();
    let Some(share) = share_opt else {
        return;
    };
    let session_id = &share.session_id;
    let tokens: Vec<String> = {
        let map = state.session_share_tokens.read().await;
        map.iter()
            .filter(|(_, r)| r.share_id == *share_id)
            .map(|(t, _)| t.clone())
            .collect()
    };
    let session_opt = state.sessions.read().await.get(session_id).cloned();
    if let Some(mut session) = session_opt {
        if let Some(viewers) = session.get_mut("viewers").and_then(|v| v.as_array_mut()) {
            viewers.retain(|v| {
                !tokens
                    .iter()
                    .any(|t| v.get("token").and_then(|x| x.as_str()) == Some(t.as_str()))
            });
        }
        let has_master = session
            .get("master_token")
            .and_then(|v| v.as_str())
            .map(|m| !m.is_empty())
            .unwrap_or(false);
        let is_collab = session
            .get("is_collaboration")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if has_master || is_collab {
            collaboration::push_token_table(state, session_id, &session).await;
        }
        *state.sessions.write().await.entry(session_id.clone()).or_insert(Value::Object(
            serde_json::Map::new(),
        )) = session;
        catalog::save_sessions(state).await;
    }
    if !tokens.is_empty() {
        let mut map = state.session_share_tokens.write().await;
        for t in &tokens {
            map.remove(t);
        }
    }
}

/// Remove expired session shares (and all of their visitor tokens). Mirrors
/// `cleanup_expired_shares` for file shares.
pub async fn cleanup_expired_session_shares(state: &AppState) {
    let now = now_unix_secs();
    let to_remove: Vec<String> = {
        let shares = state.session_shares.read().await;
        shares
            .iter()
            .filter(|(_, meta)| {
                meta.expiry_timestamp
                    .map(|exp| exp < now)
                    .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    if to_remove.is_empty() {
        return;
    }
    info!(
        "Found {} expired session share(s) to clean up.",
        to_remove.len()
    );
    for id in &to_remove {
        strip_share_tokens(state, id).await;
    }
    {
        let mut shares = state.session_shares.write().await;
        for id in &to_remove {
            shares.remove(id);
        }
    }
    save_session_shares(state).await;
    info!("Expired session share cleanup complete.");
}

// ---------------------------------------------------------------------------
// Routers
// ---------------------------------------------------------------------------

/// The owner-only, E2EE-encrypted session-share management routes. Nested
/// under `/api` (see `app.rs`), mirroring the catalog session routes.
pub fn owner_router() -> Router<AppState> {
    Router::new()
        .route(
            "/sessions/{session_id}/shares",
            post(create_session_share),
        )
        .route("/sessions/{session_id}/shares", get(list_session_shares))
        .route(
            "/sessions/{session_id}/shares/{share_id}",
            axum::routing::patch(update_session_share),
        )
        .route(
            "/sessions/{session_id}/shares/{share_id}",
            delete(revoke_session_share),
        )
}

/// The plaintext, account-less public entry routes: `GET/POST /share/{id}`.
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/share/{share_id}", get(access_session_share_get))
        .route("/share/{share_id}", post(access_session_share_post))
}

#[cfg(test)]
mod de_tests {
    use super::*;
    #[test]
    fn de_expiry_null() {
        let req: UpdateSessionShareRequest =
            serde_json::from_value(serde_json::json!({ "expiry_hours": null })).unwrap();
        assert_eq!(req.expiry_hours, Some(None), "null must be Some(None)");
        let req2: UpdateSessionShareRequest =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(req2.expiry_hours, None, "absent must be None");
        let req3: UpdateSessionShareRequest =
            serde_json::from_value(serde_json::json!({ "expiry_hours": 2 })).unwrap();
        assert_eq!(req3.expiry_hours, Some(Some(2)));
        let pw: UpdateSessionShareRequest =
            serde_json::from_value(serde_json::json!({ "password": null })).unwrap();
        assert_eq!(pw.password, Some(None), "null password must be Some(None)");
    }
}
