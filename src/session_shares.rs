//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Share-session links (ANT-305): let an owner share a running app session
//! with anyone via a link, with per-link capability toggles, an optional
//! password, an optional expiry, and per-link revocation.
//!
//! Mirrors the public file-share model in `files.rs` (`public_shares`): a
//! durable `session_shares` table, an owner-only E2EE management API under
//! `/api/sessions/{id}/shares`, and a plaintext, account-less entry point at
//! `GET/POST /share/{share_id}`. The self-serve join (ANT-440) enters through
//! the same surface via a token-free hop, `GET /join/{session_id}/{username}`
//! (ANT-466, F1): the join response opens that hop, which hands the
//! participant token over through the path-scoped `collab_token_{id}` cookie
//! and 303-redirects — `Cache-Control: private, no-store` — to the session
//! URL with the token stripped.
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
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
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
    /// The user the owner expressly invited on this link (ANT-472): a named
    /// invite lets that user see the session on the apps page and join it,
    /// where a generic (unnamed) link only admits its holder through the link
    /// itself. Existing rows deserialize to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invited_username: Option<String>,
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
    pub invited_username: Option<String>,
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
    /// The user the owner expressly invites to join the session (ANT-472):
    /// must be an existing user, not the owner. Absent = a generic link.
    #[serde(default)]
    pub username: Option<String>,
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
    /// `null` (or `""`) clears the invited user; a non-empty name sets it.
    #[serde(default, deserialize_with = "de_present_string")]
    pub username: Option<Option<String>>,
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
// Named invite (ANT-472): validation + the owner-grant check
// ---------------------------------------------------------------------------

/// Validate the `username` of a named invite: it must be an existing user and
/// must not be the session's owner. Returns the error detail when invalid.
async fn validate_invite_username(state: &AppState, username: &str, owner_username: &str) -> Option<String> {
    if username == owner_username {
        return Some("The session owner cannot be invited on the session's own share link.".to_string());
    }
    match user_manager::get_user(&state.db, username).await {
        Ok(Some(_)) => None,
        Ok(None) => Some(format!("No user named '{username}'.")),
        Err(e) => Some(format!("Could not look up user '{username}': {e}")),
    }
}

/// The owner's express grant for a session (ANT-472): the caller holds it
/// when they have a **non-revoked, non-expired named share link** for the
/// session that names them, **or** a **persisted participant entry** in the
/// session's `viewers` record (a join made after such an invite, or a
/// grandfathered pre-0.8.5 self-serve joiner, until the owner revokes). A
/// generic (unnamed) link is not a grant — its holder enters through the link
/// itself and does not light up the apps page.
pub async fn caller_has_session_grant(
    state: &AppState,
    session_id: &str,
    session: &Value,
    username: &str,
) -> bool {
    let now = now_unix_secs();
    {
        let shares = state.session_shares.read().await;
        if shares.values().any(|m| {
            m.session_id == session_id
                && m.invited_username.as_deref() == Some(username)
                && !m.revoked
                && m.expiry_timestamp
                    .map(|exp| exp >= now)
                    .unwrap_or(true)
        }) {
            return true;
        }
    }
    session
        .get("viewers")
        .and_then(|v| v.as_array())
        .map(|viewers| {
            viewers.iter().any(|v| {
                v.get("username").and_then(|u| u.as_str()) == Some(username)
                    && v.get("permission").and_then(|p| p.as_str()) == Some("participant")
            })
        })
        .unwrap_or(false)
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
    // The optional named invite (ANT-472): an existing user who is not the
    // owner; absent (or empty) keeps the link generic.
    let invited_username = match req.username.as_deref().filter(|u| !u.is_empty()) {
        Some(u) => match validate_invite_username(&state, u, &user.record.username).await {
            None => Some(u.to_string()),
            Some(detail) => {
                return e2ee::encrypt_response(
                    &state,
                    &headers,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({ "detail": detail }),
                )
                .await
            }
        },
        None => None,
    };
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
        invited_username,
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
    // Set or clear the named invite (ANT-472): a non-empty name sets it,
    // `null`/`""` clears it.
    if let Some(u) = req.username {
        match u.filter(|u| !u.is_empty()) {
            Some(u) => {
                if let Some(detail) = validate_invite_username(&state, &u, &meta.owner_username).await
                {
                    return e2ee::encrypt_response(
                        &state,
                        &headers,
                        StatusCode::UNPROCESSABLE_ENTITY,
                        serde_json::json!({ "detail": detail }),
                    )
                    .await
                }
                meta.invited_username = Some(u);
            }
            None => meta.invited_username = None,
        }
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
    let invited_username = meta.invited_username.clone();
    drop(shares);
    // Revoking strips every per-visit token this share has ever minted (from
    // the session record, the container's token table, and the vreckan-side
    // token map) and marks the row revoked so the public entry answers 410.
    strip_share_tokens(&state, &share_id).await;
    // Revoking a named invite (ANT-472) also strips the invited user's
    // participation: their participant entry in the session's viewers list,
    // the container's token table, and the vreckan-side token map.
    if let Some(invited) = &invited_username {
        strip_share_tokens_for_session(&state, &session_id, &format!("join:{invited}")).await;
    }
    {
        let mut shares = state.session_shares.write().await;
        if let Some(meta) = shares.get_mut(&share_id) {
            meta.revoked = true;
        }
    }
    save_session_shares(&state).await;
    // The revocation response returns the revoked link's info (including
    // `invited_username`) so the UI can say who just lost access.
    let revoked = state
        .session_shares
        .read()
        .await
        .get(&share_id)
        .cloned();
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        revoked.map(|m| share_info_json(&share_id, &m)).unwrap_or_else(|| {
            serde_json::json!({ "detail": "Share not found." })
        }),
    )
    .await
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
        "invited_username": meta.invited_username,
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
    // The redirect URL may carry a per-visit token (ANT-322, F5; the share
    // flow): keep that response out of any caching proxy's store so one
    // visitor's token is not served to another.
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, redirect)
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(axum::body::Body::empty())
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

/// `GET /join/{session_id}/{username}` — the token-free hop for a self-serve
/// join (ANT-466, F1). The join response hands the caller this URL instead of
/// the token-bearing session URL; this hop looks the caller's participant
/// entry up in the session's viewers list, hands the token over through the
/// path-scoped `collab_token_{id}` cookie (the same format the proxy's
/// first-hit handoff uses), and 303-redirects — `Cache-Control: private,
/// no-store` — to the session URL with the token stripped. The redirect
/// target then authenticates by cookie alone, so the token never appears in
/// the address bar or history. Dead-link states mirror the share entry: 404
/// when the session is gone, 410 when the participant entry is gone.
async fn access_session_join_hop(
    State(state): State<AppState>,
    PathParam((session_id, username)): PathParam<(String, String)>,
) -> Response {
    let session = state.sessions.read().await.get(&session_id).cloned();
    let Some(session) = session else {
        return (
            StatusCode::NOT_FOUND,
            Html("<h1>The session this join points to has ended.</h1>"),
        )
            .into_response();
    };
    let token = session
        .get("viewers")
        .and_then(|v| v.as_array())
        .and_then(|viewers| {
            viewers
                .iter()
                .find(|v| {
                    v.get("username").and_then(|u| u.as_str()) == Some(username.as_str())
                        && v.get("permission").and_then(|p| p.as_str()) == Some("participant")
                })
                .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(|t| t.to_string()))
        });
    let Some(token) = token else {
        return (
            StatusCode::GONE,
            Html("<h1>This join has ended.</h1>"),
        )
            .into_response();
    };
    let set_cookie = format!(
        "collab_token_{session_id}={token}; Path=/api/apps/session/{session_id}; HttpOnly; Secure; SameSite=none"
    );
    let mut response = see_other(format!("/api/apps/session/{session_id}/"));
    if let Ok(hv) = HeaderValue::from_str(&set_cookie) {
        response.headers_mut().append(header::SET_COOKIE, hv);
    }
    response
}

// ---------------------------------------------------------------------------
// Granting access (per-visit tokens + lazy secure-mode promotion)
// ---------------------------------------------------------------------------

/// The details of one visit to be recorded: the visitor's identity (their
/// real username for a join, a random `User-NNN` for a share), the
/// permission the visit carries, the capability snapshot, and the share that
/// granted the visit (a real share id, or a synthetic `join:{user}` id for a
/// self-serve join).
struct VisitGrant {
    username: String,
    permission: &'static str,
    caps: SessionShareCaps,
    share_id: String,
}

/// The mint core shared by the share-visit grant (ANT-305) and the
/// self-serve join (ANT-440): record the per-visit token on the vreckan side
/// (WP3 resolves a visitor's caps through this map — the caps snapshot is
/// what gives a mid-visit cap edit its "next open" semantics, ANT-306), add
/// the visitor to the session's viewers list (a plain session with input
/// gets a controller entry in the token table; a room keeps the
/// MK/slot-provided "viewer" role), lazily promote a plain session to
/// selkies secure mode when the container supports it, and persist the
/// session. Returns true when the session is in secure mode after the grant.
///
/// A join is exactly a share grant whose visitor is the caller themselves
/// (their real username, full participant caps), so both flows funnel
/// through here.
async fn apply_visit_to_session(
    state: &AppState,
    session_id: &str,
    session: &mut Value,
    visit_token: &str,
    grant: &VisitGrant,
) -> bool {
    let is_collab = session
        .get("is_collaboration")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Record the per-visit token on the vreckan side. For a legacy
    // (non-secure) plain session this map is the only place the proxy knows
    // the token, so it is what admits the visit there.
    {
        let mut tokens = state.session_share_tokens.write().await;
        tokens.insert(
            visit_token.to_string(),
            SessionShareTokenRecord {
                share_id: grant.share_id.clone(),
                session_id: session_id.to_string(),
                caps: grant.caps.clone(),
            },
        );
    }

    // Add the visitor to the session's viewers list. In a room session the
    // container role stays "viewer" (the room's MK/slot mechanism provides
    // input for participants); a full share or join on a plain session gets
    // a controller entry in the token table instead. A plain session that
    // has not yet been promoted to secure mode carries no master_token and so
    // no viewers array yet — ensure one exists before pushing.
    if !session.get("viewers").map(|v| v.is_array()).unwrap_or(false) {
        session["viewers"] = Value::Array(vec![]);
    }
    if let Some(viewers) = session.get_mut("viewers").and_then(|v| v.as_array_mut()) {
        viewers.push(serde_json::json!({
            "token": visit_token,
            "slot": Value::Null,
            "mk_control": false,
            "username": grant.username,
            "permission": grant.permission,
        }));
        if !is_collab && grant.caps.input {
            if let Some(v) = viewers
                .iter_mut()
                .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(visit_token))
            {
                v["role"] = Value::String("controller".to_string());
            }
        }
    }

    // Lazy secure-mode promotion: for a plain session, mint a master token
    // and push the full token table to the container. The push happens on
    // every visit (not only the first) so a token added after launch — a
    // share or a join, including a re-join after a recreate — reaches the
    // container's table. When the container has no token control plane
    // (older selkies images answer 404 / are unreachable) the session stays
    // legacy and the visitor's role rides the `?role=` query parameter
    // instead.
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
        if collaboration::push_token_table(state, session_id, session).await {
            promoted = true;
        } else if !had_master {
            // The push failed: this container isn't in secure mode. Undo the
            // mint (and the controller role, which only means something to a
            // secure-mode container).
            session.as_object_mut().map(|o| o.remove("master_token"));
            if let Some(viewers) = session.get_mut("viewers").and_then(|v| v.as_array_mut()) {
                if let Some(v) = viewers
                    .iter_mut()
                    .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(visit_token))
                {
                    v.as_object_mut().map(|o| o.remove("role"));
                }
            }
        }
    }

    *state.sessions.write().await.entry(session_id.to_string()).or_insert(Value::Object(
        serde_json::Map::new(),
    )) = session.clone();
    catalog::save_sessions(state).await;
    promoted
}

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
    // Share visitors are anonymous on purpose (the share link, not the
    // person, is the credential): a random per-visit username.
    let grant = VisitGrant {
        username: format!("User-{}", rand::rng().random_range(100..999)),
        permission,
        caps: share.caps.clone(),
        share_id: share_id.to_string(),
    };

    let promoted = apply_visit_to_session(state, session_id, &mut session, &visit_token, &grant)
        .await;
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

/// The full participant capability set a self-serve join is granted: input
/// plus the in-container file API (a join is an explicit "I'm here to use
/// this", not a passive share view).
pub fn join_participant_caps() -> SessionShareCaps {
    SessionShareCaps {
        input: true,
        file_browse: true,
        file_download: true,
        file_upload: true,
    }
}

/// Grant a self-serve join (ANT-440) to a running plain session: mint the
/// caller their own participant token (their real username, full
/// participant caps), record it in the session's viewers list and the
/// container's token table, and return the token-free join hop URL that
/// admits them (ANT-466, F1: the `GET /join/{session_id}/{username}` hop
/// hands the token over via the `collab_token_{id}` cookie and redirects to
/// the session URL with the token stripped, so the token never lands in the
/// address bar).
///
/// Idempotent per (session, user): a re-join returns the caller's existing
/// participant token and re-establishes it in the container (closing the
/// recreate gap) rather than minting a second one.
///
/// Returns `None` when the session is gone. Validation (plain vs room,
/// caller's app access) happens in the route handler so it can answer with
/// distinct statuses.
pub async fn grant_session_join_access(
    state: &AppState,
    session_id: &str,
    username: &str,
) -> Option<String> {
    let mut session = state.sessions.read().await.get(session_id).cloned()?;
    let is_collab = session
        .get("is_collaboration")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Idempotent reuse: the callers list is the durable source of truth (it
    // is persisted with the session), so a re-join finds the existing
    // participant entry even across a vreckan restart.
    let existing_token = session
        .get("viewers")
        .and_then(|v| v.as_array())
        .and_then(|viewers| {
            viewers
                .iter()
                .find(|v| {
                    v.get("username").and_then(|u| u.as_str()) == Some(username)
                        && v.get("permission").and_then(|p| p.as_str()) == Some("participant")
                })
                .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(|t| t.to_string()))
        });
    if let Some(visit_token) = existing_token {
        // Re-admit on the vreckan side (legacy sessions) and re-establish in
        // the container (a recreate rebuilt the table from the original
        // launch tokens, dropping later joins).
        {
            let mut tokens = state.session_share_tokens.write().await;
            tokens.insert(
                visit_token.clone(),
                SessionShareTokenRecord {
                    share_id: format!("join:{username}"),
                    session_id: session_id.to_string(),
                    caps: join_participant_caps(),
                },
            );
        }
        if !is_collab {
            let _ = collaboration::push_token_table(state, session_id, &session).await;
        }
        return Some(join_hop_url(session_id, username));
    }

    let visit_token = catalog::random_token(16);
    // A synthetic share id (real share ids are UUIDs) so the vreckan-side
    // token map admits the joiner for legacy (non-secure) sessions.
    let grant = VisitGrant {
        username: username.to_string(),
        permission: "participant",
        caps: join_participant_caps(),
        share_id: format!("join:{username}"),
    };
    apply_visit_to_session(state, session_id, &mut session, &visit_token, &grant).await;
    info!("[{session_id}] Self-serve join: granted participant ({username})");
    Some(join_hop_url(session_id, username))
}

/// The token-free URL a self-serve join opens (ANT-466, F1): the
/// `GET /join/{session_id}/{username}` hop. Keyed on the caller's username —
/// letters, numbers, underscores, or hyphens, so path-safe by construction
/// (`user_manager::valid_name`) — and resolved from the session's viewers
/// list, so no separate nonce store is needed and a replayed or bookmarked
/// hop keeps working across vreckan restarts.
fn join_hop_url(session_id: &str, username: &str) -> String {
    format!("/join/{session_id}/{username}")
}

/// Strip every per-visit token minted by the share with the given id: from
/// the session's viewers list, the container's token table (when one exists),
/// and the vreckan-side token map. Used by revocation and expiry cleanup.
/// The session is resolved from the share row.
pub async fn strip_share_tokens(state: &AppState, share_id: &str) {
    let session_id = state
        .session_shares
        .read()
        .await
        .get(share_id)
        .map(|m| m.session_id.clone());
    let Some(session_id) = session_id else {
        return;
    };
    strip_share_tokens_for_session(state, &session_id, share_id).await;
}

/// The session-scoped core of [`strip_share_tokens`]: given the session the
/// share belongs to, strip the share's tokens from the session's viewers
/// list, the container's token table (when one exists), and the vreckan-side
/// token map. Works for synthetic share ids too (the `join:{username}` id a
/// self-serve join mints under, ANT-472's strip-on-revoke).
pub async fn strip_share_tokens_for_session(state: &AppState, session_id: &str, share_id: &str) {
    // Note: clone out of each lock in a plain statement. A lock guard
    // temporary held across an `if let` scrutinee survives until the end of
    // the block and would deadlock against the re-lock below (tokio locks
    // are not reentrant).
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
        *state.sessions.write().await.entry(session_id.to_string()).or_insert(Value::Object(
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
    let to_remove: Vec<(String, Option<String>, String)> = {
        let shares = state.session_shares.read().await;
        shares
            .iter()
            .filter(|(_, meta)| {
                meta.expiry_timestamp
                    .map(|exp| exp < now)
                    .unwrap_or(false)
            })
            .map(|(id, meta)| {
                (
                    id.clone(),
                    meta.invited_username.clone(),
                    meta.session_id.clone(),
                )
            })
            .collect()
    };
    if to_remove.is_empty() {
        return;
    }
    info!(
        "Found {} expired session share(s) to clean up.",
        to_remove.len()
    );
    for (id, invited, session_id) in &to_remove {
        strip_share_tokens(state, id).await;
        // An expired named invite (ANT-472) also strips the invited user's
        // participation entry (viewers list, container token table, and the
        // vreckan-side token map).
        if let Some(invited) = invited {
            strip_share_tokens_for_session(state, session_id, &format!("join:{invited}")).await;
        }
    }
    {
        let mut shares = state.session_shares.write().await;
        for (id, _, _) in &to_remove {
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

/// The plaintext, account-less public entry routes: `GET/POST /share/{id}`
/// and the self-serve join hop `GET /join/{session_id}/{username}`.
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/share/{share_id}", get(access_session_share_get))
        .route("/share/{share_id}", post(access_session_share_post))
        .route(
            "/join/{session_id}/{username}",
            get(access_session_join_hop),
        )
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
