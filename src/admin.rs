//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Admin management endpoints: roster data, global settings, app stores,
//! meta apps, app installation/updates, templates, session oversight, OIDC
//! revocation, people/groups, volume mounts, and roles.
//!
//! Ports `server/routers/admin`. Each endpoint declares the specific
//! permission it requires (via `require_permission`), so a user who holds,
//! say, only `admin.apps` can manage apps but not accounts. The frontend
//! mirrors this by only showing the admin sections the caller has permission
//! for. The `/certificates/*` and `/backup/*` route groups are added by
//! `acme_orchestrator.rs` and `backup_manager.rs` respectively.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{OriginalUri, Path as PathParam, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::Engine;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter};
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use crate::auth::{self, AuthenticatedUser};
use crate::catalog;
use crate::docker_provider::DockerProvider;
use crate::e2ee;
use crate::entities;
use crate::permissions;
use crate::state::AppState;
use crate::user_manager;

/// Test hook: true when the `Db.fail_writes` simulation is armed, so the
/// handlers below treat raw SeaORM writes as failed even when the database
/// itself is healthy (integration tests arm it to exercise the error paths).
fn db_fails_writes(state: &AppState) -> bool {
    state.db.fail_writes.load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Permission catalog descriptions
// ---------------------------------------------------------------------------

/// Human-readable descriptions for the permission catalog, surfaced by
/// `GET /api/admin/permissions` so the role editor can label its checkboxes.
const PERMISSION_DESCRIPTIONS: &[(&str, &str)] = &[
    ("admin", "Super-permission: grants every other permission."),
    ("admin.accounts", "Manage accounts (create, edit, delete, passwords, access)."),
    ("admin.groups", "Manage groups and SSO group configuration."),
    ("admin.roles", "Manage roles and the permission catalog."),
    ("admin.apps", "Manage installed apps (install, update, delete, launch)."),
    ("admin.stores", "Manage app stores and browse available apps."),
    ("admin.templates", "Manage app templates and the template schema."),
    ("admin.mounts", "Manage external volume mounts."),
    ("admin.backup", "Manage backups (create, restore, schedule)."),
    ("admin.sessions", "View and stop all sessions; revoke SSO sessions."),
    ("admin.laboratory", "Use the laboratory (live session console)."),
    ("admin.global_settings", "Edit the global server settings."),
    ("admin.sso", "Configure SSO / OIDC (provider connection and group mapping)."),
    ("admin.certificates", "Manage TLS certificates (Let's Encrypt issuance and renewal)."),
    ("user.storage", "Use persistent storage (home directories, uploads)."),
    ("user.sharing", "Create public file shares."),
    ("user.gpu", "Use GPUs when launching apps."),
    ("user.harden_container", "Harden app containers."),
    ("user.harden_openbox", "Harden the Openbox desktop."),
];

fn permission_description(perm: &str) -> &'static str {
    PERMISSION_DESCRIPTIONS
        .iter()
        .find(|(p, _)| *p == perm)
        .map(|(_, d)| *d)
        .unwrap_or("")
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// The SSO page's save payload. Group lists are sent as arrays (the UI uses
/// tag inputs); they are stored comma-joined, matching the env format.
#[derive(Deserialize)]
struct SsoConfigRequest {
    oidc_enabled: bool,
    #[serde(default)]
    oidc_issuer: String,
    #[serde(default)]
    oidc_client_id: String,
    #[serde(default)]
    oidc_client_secret: String,
    #[serde(default)]
    oidc_redirect_uri: String,
    #[serde(default = "default_scopes")]
    oidc_scopes: String,
    #[serde(default = "default_true")]
    oidc_allow_signup: bool,
    #[serde(default = "default_state_ttl")]
    oidc_state_ttl_seconds: i64,
    #[serde(default)]
    oidc_admin_groups: Vec<String>,
    #[serde(default)]
    oidc_user_groups: Vec<String>,
}

fn default_scopes() -> String {
    "openid email profile".to_string()
}
fn default_state_ttl() -> i64 {
    600
}
fn default_true() -> bool {
    true
}
#[allow(dead_code)]
fn default_false() -> bool {
    false
}

/// The Certificates page's save payload. `acme_domains` is sent as an array; it
/// is stored comma-joined. The Cloudflare token is only overwritten when a
/// non-empty value is posted.
#[derive(Deserialize)]
struct CertificateConfigRequest {
    acme_enabled: bool,
    #[serde(default = "default_acme_ca")]
    acme_ca: String,
    #[serde(default)]
    acme_allow_http_ca: bool,
    #[serde(default)]
    acme_domains: Vec<String>,
    #[serde(default = "default_acme_auth")]
    acme_auth: String,
    #[serde(default)]
    acme_cloudflare_email: String,
    #[serde(default)]
    acme_cloudflare_token: String,
    #[serde(default)]
    acme_staging: bool,
    #[serde(default = "default_true")]
    acme_auto_renew: bool,
    #[serde(default = "default_renew_hours")]
    acme_renew_check_hours: i64,
    #[serde(default = "default_renew_days")]
    acme_renew_before_days: i64,
    #[serde(default = "default_acme_dns_provider")]
    acme_dns_provider: String,
    #[serde(default)]
    acme_cloudflare_zone_id: String,
    #[serde(default)]
    acme_dns_provider_config: String,
}

fn default_acme_ca() -> String {
    "letsencrypt".to_string()
}
fn default_acme_auth() -> String {
    "dns-cloudflare".to_string()
}
fn default_acme_dns_provider() -> String {
    "cloudflare".to_string()
}
fn default_renew_hours() -> i64 {
    6
}
fn default_renew_days() -> i64 {
    30
}

/// Body for `POST /api/admin/apps/meta`. Mirrors `CreateMetaAppRequest`.
#[derive(Deserialize)]
struct CreateMetaAppRequest {
    name: String,
    base_app_id: String,
    logo: String,
    #[serde(default)]
    custom_autostart_script_b64: Option<String>,
    #[serde(default)]
    custom_autostart_wayland_script_b64: Option<String>,
    #[serde(default)]
    users: Vec<String>,
    #[serde(default)]
    groups: Vec<String>,
}

/// Body for `POST /api/admin/launch/meta_customize`. Mirrors
/// `LaunchMetaCustomizeRequest`.
#[derive(Deserialize)]
struct LaunchMetaCustomizeRequest {
    application_id: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    selected_gpu: Option<String>,
    #[serde(default = "default_true")]
    wayland_mode: bool,
}

/// Body for `POST /api/admin/people`. Mirrors `CreatePersonRequest`.
#[derive(Deserialize)]
struct CreatePersonRequest {
    username: String,
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    is_admin: bool,
}

/// Body for `PUT /api/admin/people/{username}`. Mirrors `UpdateUserRequest`.
#[derive(Deserialize)]
struct UpdateUserRequest {
    settings: Value,
}

/// Body for `POST /api/admin/people/{username}/admin-status`. Mirrors
/// `SetAdminStatusRequest`.
#[derive(Deserialize)]
struct SetAdminStatusRequest {
    is_admin: bool,
}

/// Body for `POST /api/admin/people/{username}/access`. Mirrors
/// `SetUserAccessRequest`.
#[derive(Deserialize)]
struct SetUserAccessRequest {
    #[serde(default)]
    roles: Option<Vec<String>>,
    #[serde(default)]
    permissions: Option<Vec<String>>,
    #[serde(default)]
    groups: Option<Vec<String>>,
}

/// Body for `POST /api/admin/groups`. Mirrors `CreateGroupRequest`.
#[derive(Deserialize)]
struct CreateGroupRequest {
    name: String,
    settings: Value,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

/// Body for `PUT /api/admin/groups/{group_name}`. Mirrors `UpdateGroupRequest`.
#[derive(Deserialize)]
struct UpdateGroupRequest {
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    roles: Option<Vec<String>>,
    #[serde(default)]
    permissions: Option<Vec<String>>,
}

/// Body for `POST /api/admin/volume_mounts`. Mirrors `CreateVolumeMountRequest`.
#[derive(Deserialize)]
struct CreateVolumeMountRequest {
    name: String,
    host_path: String,
    container_path: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default)]
    target: String,
}

/// Body for `PUT /api/admin/volume_mounts/{name}`. Mirrors
/// `UpdateVolumeMountRequest`.
#[derive(Deserialize)]
struct UpdateVolumeMountRequest {
    host_path: String,
    container_path: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default)]
    target: String,
}

fn default_scope() -> String {
    "none".to_string()
}

/// Body for `POST /api/admin/roles`. Mirrors `CreateRoleRequest`.
#[derive(Deserialize)]
struct CreateRoleRequest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    permissions: Vec<String>,
}

/// Body for `PUT /api/admin/roles/{role_name}`. Mirrors `UpdateRoleRequest`.
#[derive(Deserialize)]
struct UpdateRoleRequest {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    permissions: Option<Vec<String>>,
}

/// Body for `POST /api/admin/oidc/revoke`. Mirrors `RevokeOidcSessionRequest`.
#[derive(Deserialize)]
struct RevokeOidcSessionRequest {
    username: String,
}

/// Body for `POST /api/admin/people/{username}/homedirs`. Mirrors
/// `HomeDirectoryCreate`.
#[derive(Deserialize)]
struct HomeDirectoryCreate {
    home_name: String,
}

/// Query params for `GET /api/admin/apps/available`.
#[derive(Deserialize)]
struct AvailableAppsQuery {
    url: String,
    store_name: String,
    #[serde(default)]
    refresh: bool,
}

/// Query params for `GET /api/admin/backup/{name}/download`.
#[derive(Deserialize)]
struct BackupDownloadQuery {
    #[serde(default)]
    chunk_index: i64,
}

/// Body for `POST /api/admin/backup/create`. Mirrors `CreateBackupRequest`.
#[derive(Deserialize)]
struct CreateBackupRequest {
    #[serde(default)]
    include_user_data: bool,
}

/// Body for `POST /api/admin/backup/restore/chunk`. Mirrors
/// `BackupRestoreChunkRequest`.
#[derive(Deserialize)]
struct BackupRestoreChunkRequest {
    upload_id: String,
    chunk_index: i64,
    chunk_data_b64: String,
}

/// Body for `POST /api/admin/backup/restore/finalize`. Mirrors
/// `BackupRestoreFinalizeRequest`.
#[derive(Deserialize)]
struct BackupRestoreFinalizeRequest {
    upload_id: String,
    total_chunks: i64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// True if the caller's effective permissions include `perm`.
fn require_permission(user: &AuthenticatedUser, perm: &str) -> bool {
    user.permissions.iter().any(|p| p == perm)
}

/// True if the user holds at least one `admin.*` permission (or the `admin`
/// super-permission). Used to gate the aggregate `/data` endpoint.
fn has_any_admin_permission(user: &AuthenticatedUser) -> bool {
    user.permissions
        .iter()
        .any(|p| p.starts_with("admin.") || *p == "admin")
}

/// Build a roster entry (the `User` view) from a `UserRecord`, filling any
/// missing settings keys with the defaults so the roster form always receives
/// a complete settings object. Mirrors `_roster_entry`.
fn roster_entry(u: &user_manager::UserRecord) -> Value {
    let mut filled = user_manager::default_user_settings().clone();
    if let Some(raw) = &u.settings {
        if let (Some(f_obj), Some(r_obj)) = (filled.as_object_mut(), raw.as_object()) {
            for (k, v) in r_obj {
                f_obj.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::json!({
        "username": u.username,
        "is_admin": u.is_admin,
        "settings": filled,
        "is_sso": u.is_sso,
        "has_password": u.password_hash.is_some(),
        "sso_groups": u.sso_groups,
        "roles": u.roles,
        "permissions": u.permissions,
        "groups": u.groups,
    })
}

/// The SSO setting field names (for the per-field source report).
const SSO_SETTING_FIELDS: &[&str] = &[
    "oidc_enabled",
    "oidc_issuer",
    "oidc_client_id",
    "oidc_client_secret",
    "oidc_redirect_uri",
    "oidc_scopes",
    "oidc_allow_signup",
    "oidc_state_ttl_seconds",
    "oidc_admin_groups",
    "oidc_user_groups",
];

/// The Let's Encrypt setting field names (for the per-field source report).
const ACME_SETTING_FIELDS: &[&str] = &[
    "acme_enabled",
    "acme_ca",
    "acme_allow_http_ca",
    "acme_domains",
    "acme_auth",
    "acme_cloudflare_email",
    "acme_cloudflare_token",
    "acme_staging",
    "acme_auto_renew",
    "acme_renew_check_hours",
    "acme_renew_before_days",
    "acme_dns_provider",
    "acme_cloudflare_zone_id",
    "acme_dns_provider_config",
];

/// Report where a setting's current value comes from: `"db"` when a value is
/// stored in the `app_settings` table (edited in the UI), `"env"` when it
/// comes from the environment, or `"default"` when neither is set. Mirrors
/// `settings.setting_source`.
async fn setting_source(state: &AppState, name: &str) -> String {
    let env_var = format!("VRECKAN_{}", name.to_uppercase());
    if state.db.get_app_setting(&env_var).await.ok().flatten().is_some() {
        return "db".to_string();
    }
    if state.settings.raw_value(name).is_some() {
        return "env".to_string();
    }
    "default".to_string()
}

/// Split a comma/space-separated group list into a `Vec<String>`.
fn split_groups(s: &str) -> Vec<String> {
    s.replace(',', " ")
        .split_whitespace()
        .map(|t| t.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Roster data + global settings
// ---------------------------------------------------------------------------

/// `POST /api/admin/data` — the whole management payload (roster, groups,
/// roles, mounts, ports, GPUs). Gated on "any admin permission".
async fn get_management_data(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !has_any_admin_permission(&user) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let admins = user_manager::get_all_admins(&state.db).await.unwrap_or_default();
    let users = user_manager::get_all_users(&state.db).await.unwrap_or_default();
    let groups = user_manager::get_all_groups(&state.db).await.unwrap_or_default();
    let roles = user_manager::get_all_roles(&state.db).await.unwrap_or_default();
    let volume_mounts = list_volume_mounts_value(&state).await;
    let gpus = state.available_gpus.read().await.clone();
    let value = serde_json::json!({
        "admins": admins.iter().map(roster_entry).collect::<Vec<_>>(),
        "users": users.iter().map(roster_entry).collect::<Vec<_>>(),
        "groups": groups,
        "roles": roles,
        "volume_mounts": volume_mounts,
        "api_port": state.settings.api_port,
        "session_port": state.settings.session_port,
        "gpus": gpus,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `GET /api/admin/global-settings` — the global default GPU + available GPUs.
async fn get_global_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.global_settings") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let gpus = state.available_gpus.read().await.clone();
    let value = serde_json::json!({
        "global_default_gpu": state.global_default_gpu.read().await.clone(),
        "gpus": gpus,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/global-settings` — save the global default GPU.
async fn save_global_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.global_settings") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let body = if decrypted.is_null() { Value::Object(serde_json::Map::new()) } else { decrypted };
    // An absent key means "leave unchanged"; an empty string clears it.
    let gpu_value = body.get("global_default_gpu");
    if let Some(gpu_value) = gpu_value {
        let device = gpu_value.as_str().unwrap_or("").trim().to_string();
        let gpus = state.available_gpus.read().await.clone();
        if !device.is_empty() && !gpus.iter().any(|g| g.device == device) {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "detail": format!("Unknown GPU device: '{device}'.") }),
            )
            .await;
        }
        if device.is_empty() {
            // Clear: delete the row.
            let am = entities::app_setting::ActiveModel {
                key: ActiveValue::Set("global_default_gpu".to_string()),
                value: ActiveValue::NotSet,
            };
            if db_fails_writes(&state)
                || entities::app_setting::Entity::delete(am).exec(&state.db.conn).await.is_err()
            {
                return e2ee::encrypt_response(
                    &state,
                    &headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "detail": "Failed to save global settings." }),
                )
                .await;
            }
            *state.global_default_gpu.write().await = None;
        } else if state.db.set_app_setting("global_default_gpu", &device).await.is_err() {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": "Failed to save global settings." }),
            )
            .await;
        } else {
            *state.global_default_gpu.write().await = Some(device.clone());
        }
        let gpu_msg = if device.is_empty() {
            "cleared".to_string()
        } else {
            format!("set to {device}")
        };
        info!("Global default GPU {gpu_msg}.");
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::json!({ "ok": true })).await
}

// ---------------------------------------------------------------------------
// SSO / OIDC configuration
// ---------------------------------------------------------------------------

/// `GET /api/admin/sso-groups` — the SSO group configuration (read-only) plus
/// the groups SSO users are actually in.
async fn get_sso_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.groups") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let admin_groups = split_groups(&state.settings.oidc_admin_groups);
    let user_groups = split_groups(&state.settings.oidc_user_groups);
    let observed = user_manager::observed_sso_groups(&state.db).await.unwrap_or_default();
    let value = serde_json::json!({
        "admin_groups": admin_groups,
        "user_groups": user_groups,
        "observed": observed,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `GET /api/admin/sso` — the effective SSO/OIDC configuration plus the source
/// of each field. The client secret is masked.
async fn get_sso_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.sso") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    // The effective values are resolved DB-over-env-over-default (mirrors the
    // Python `_oidc_config()`), so a value saved on the SSO page is echoed
    // back here rather than the raw environment value.
    let cfg = auth::oidc_config(&state).await;
    let secret = cfg.client_secret.clone();
    let values = serde_json::json!({
        "oidc_enabled": cfg.enabled,
        "oidc_issuer": cfg.issuer,
        "oidc_client_id": cfg.client_id,
        "oidc_client_secret": "",
        "secret_set": !secret.is_empty(),
        "secret_length": secret.len(),
        "oidc_redirect_uri": cfg.redirect_uri,
        "oidc_scopes": cfg.scopes,
        "oidc_allow_signup": cfg.allow_signup,
        "oidc_state_ttl_seconds": cfg.state_ttl,
        "oidc_admin_groups": split_groups(&cfg.admin_groups),
        "oidc_user_groups": split_groups(&cfg.user_groups),
    });
    let mut sources = serde_json::Map::new();
    for name in SSO_SETTING_FIELDS {
        sources.insert((*name).to_string(), Value::String(setting_source(&state, name).await));
    }
    let issuer = cfg.issuer.trim().trim_end_matches('/');
    let enabled = cfg.enabled && !issuer.is_empty() && !cfg.client_id.is_empty();
    let value = serde_json::json!({
        "values": values,
        "sources": Value::Object(sources),
        "enabled": enabled,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `PUT /api/admin/sso` — save the SSO configuration to the `app_settings`
/// table (overriding the environment). Group lists are stored comma-joined.
async fn save_sso_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.sso") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: SsoConfigRequest = match serde_json::from_value(decrypted) {
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
    let payload: HashMap<&str, String> = HashMap::from([
        ("oidc_enabled", if req.oidc_enabled { "true" } else { "false" }.to_string()),
        ("oidc_issuer", req.oidc_issuer.trim().to_string()),
        ("oidc_client_id", req.oidc_client_id.trim().to_string()),
        ("oidc_client_secret", req.oidc_client_secret.clone()),
        ("oidc_redirect_uri", req.oidc_redirect_uri.trim().to_string()),
        ("oidc_scopes", req.oidc_scopes.trim().to_string()),
        ("oidc_allow_signup", if req.oidc_allow_signup { "true" } else { "false" }.to_string()),
        ("oidc_state_ttl_seconds", req.oidc_state_ttl_seconds.to_string()),
        ("oidc_admin_groups", req.oidc_admin_groups.join(",")),
        ("oidc_user_groups", req.oidc_user_groups.join(",")),
    ]);
    for name in SSO_SETTING_FIELDS {
        let value = match payload.get(*name) {
            Some(v) => v.clone(),
            None => continue,
        };
        if *name == "oidc_client_secret" && value.is_empty() {
            // Blank secret = "leave the existing one"; skip the write.
            continue;
        }
        let env_var = format!("VRECKAN_{}", name.to_uppercase());
        if let Err(e) = state.db.set_app_setting(&env_var, &value).await {
            // Log the raw DB error (constraint/table names) and send a
            // generic detail, matching the sibling endpoints — the error
            // text must not reach the client (ANT-276).
            warn!("Failed to save SSO setting {name}: {e}");
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": format!("Failed to save SSO setting {name}.") }),
            )
            .await;
        }
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::json!({ "ok": true })).await
}

/// `POST /api/admin/sso/test` — validate the configured issuer by fetching its
/// OIDC discovery document (server-side).
async fn test_sso_connection(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.sso") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    // Resolve the issuer DB-over-env (mirrors the Python `_oidc_config()`), so
    // a value saved on the SSO page is used for the discovery fetch.
    let cfg = auth::oidc_config(&state).await;
    let issuer = cfg.issuer.trim().trim_end_matches('/');
    if issuer.is_empty() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "No issuer URL configured." }),
        )
        .await;
    }
    // Reuse the same discovery path as the auth flow (mirrors the Python
    // `_oidc_discovery`, which the SSO test endpoint imports).
    let doc = match auth::oidc_discovery(issuer).await {
        Some(d) => d,
        None => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_GATEWAY,
                serde_json::json!({ "detail": "Unable to reach the OIDC provider (discovery)." }),
            )
            .await
        }
    };
    // Prefer the configured redirect_uri (DB-over-env); otherwise derive it
    // from the request's public base URL (mirrors the Python `_oidc_redirect_uri`).
    let redirect_uri = if cfg.redirect_uri.trim().is_empty() {
        let scheme = auth::request_scheme(&headers);
        let host = auth::public_host(&headers, &uri);
        format!("{scheme}://{host}/api/auth/oidc/callback")
    } else {
        cfg.redirect_uri.trim().to_string()
    };
    let value = serde_json::json!({
        "ok": true,
        "issuer": doc.get("issuer").cloned().unwrap_or(Value::Null),
        "authorization_endpoint": doc.get("authorization_endpoint").cloned().unwrap_or(Value::Null),
        "token_endpoint": doc.get("token_endpoint").cloned().unwrap_or(Value::Null),
        "userinfo_endpoint": doc.get("userinfo_endpoint").cloned().unwrap_or(Value::Null),
        "jwks_uri": doc.get("jwks_uri").cloned().unwrap_or(Value::Null),
        "redirect_uri": redirect_uri,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

// ---------------------------------------------------------------------------
// Certificates (Let's Encrypt / certbot)
// ---------------------------------------------------------------------------

/// `GET /api/admin/certificates` — the effective Let's Encrypt configuration,
/// the source of each field, and the current certificate status.
async fn get_certificates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.certificates") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    // The *effective* config (DB-over-env-over-default), mirroring Python's
    // `get_certificates` which reads via `acme_orchestrator.effective_acme_config()`.
    let cfg = crate::acme_orchestrator::effective_acme_config(&state).await;
    let domains: Vec<String> = cfg
        .acme_domains
        .split(',')
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    let token = cfg.acme_cloudflare_token.clone();
    let provider_config = cfg.acme_dns_provider_config.clone();
    let values = serde_json::json!({
        "acme_enabled": cfg.acme_enabled,
        "acme_ca": cfg.acme_ca,
        "acme_allow_http_ca": cfg.acme_allow_http_ca,
        "acme_domains": domains,
        "acme_auth": cfg.acme_auth,
        "acme_cloudflare_email": cfg.acme_cloudflare_email,
        "acme_cloudflare_token": "",
        "token_set": !token.is_empty(),
        "token_length": token.len(),
        "acme_staging": cfg.acme_staging,
        "acme_auto_renew": cfg.acme_auto_renew,
        "acme_renew_check_hours": cfg.acme_renew_check_hours,
        "acme_renew_before_days": cfg.acme_renew_before_days,
        "acme_dns_provider": cfg.acme_dns_provider,
        "acme_cloudflare_zone_id": cfg.acme_cloudflare_zone_id,
        // Masked like the Cloudflare token: the raw JSON carries the provider's
        // api_token/client_secret, which we do not want in the admin's browser
        // (ANT-275). Blank on save = keep the existing config.
        "acme_dns_provider_config": "",
        "dns_provider_config_set": !provider_config.is_empty(),
        "dns_provider_config_length": provider_config.len(),
    });
    let mut sources = serde_json::Map::new();
    for name in ACME_SETTING_FIELDS {
        sources.insert((*name).to_string(), Value::String(setting_source(&state, name).await));
    }
    let mut status = crate::acme_orchestrator::cert_status(&state, &domains).await;
    // Elide the private-key PEM from the admin's response (ANT-275).
    crate::acme_orchestrator::mask_status_key(&mut status);
    let value = serde_json::json!({
        "values": values,
        "sources": Value::Object(sources),
        "status": status,
        "dns_providers": crate::acme::dns_provider_choices(),
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `PUT /api/admin/certificates` — save the Let's Encrypt configuration to the
/// `app_settings` table (overriding the environment).
async fn save_certificates(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.certificates") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: CertificateConfigRequest = match serde_json::from_value(decrypted) {
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
    let payload: HashMap<&str, String> = HashMap::from([
        ("acme_enabled", if req.acme_enabled { "true" } else { "false" }.to_string()),
        ("acme_ca", if req.acme_ca.trim().is_empty() { "letsencrypt".to_string() } else { req.acme_ca.trim().to_string() }),
        ("acme_allow_http_ca", if req.acme_allow_http_ca { "true" } else { "false" }.to_string()),
        ("acme_domains", req.acme_domains.iter().map(|d| d.trim()).filter(|d| !d.is_empty()).collect::<Vec<_>>().join(",")),
        ("acme_auth", if req.acme_auth.trim().is_empty() { "dns-cloudflare".to_string() } else { req.acme_auth.trim().to_string() }),
        ("acme_cloudflare_email", req.acme_cloudflare_email.trim().to_string()),
        ("acme_cloudflare_token", req.acme_cloudflare_token.clone()),
        ("acme_staging", if req.acme_staging { "true" } else { "false" }.to_string()),
        ("acme_auto_renew", if req.acme_auto_renew { "true" } else { "false" }.to_string()),
        ("acme_renew_check_hours", req.acme_renew_check_hours.to_string()),
        ("acme_renew_before_days", req.acme_renew_before_days.to_string()),
        ("acme_dns_provider", if req.acme_dns_provider.trim().is_empty() { "cloudflare".to_string() } else { req.acme_dns_provider.trim().to_string() }),
        ("acme_cloudflare_zone_id", req.acme_cloudflare_zone_id.trim().to_string()),
        ("acme_dns_provider_config", req.acme_dns_provider_config.trim().to_string()),
    ]);
    for name in ACME_SETTING_FIELDS {
        let value = match payload.get(*name) {
            Some(v) => v.clone(),
            None => continue,
        };
        if (*name == "acme_cloudflare_token" || *name == "acme_dns_provider_config") && value.is_empty() {
            // Blank secret = "leave the existing one"; skip the write.
            // (Both are masked to blank in the GET response, ANT-275.)
            continue;
        }
        let env_var = format!("VRECKAN_{}", name.to_uppercase());
        if let Err(e) = state.db.set_app_setting(&env_var, &value).await {
            warn!("Failed to save ACME setting {name}: {e}");
        }
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::json!({ "ok": true })).await
}

/// `POST /api/admin/certificates/renew` — issue (if no valid cert exists) or
/// renew the Let's Encrypt certificate now.
async fn renew_certificates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.certificates") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    // The *effective* config (DB-over-env-over-default), mirroring Python's
    // `renew_certificates` which reads via `acme_orchestrator.effective_acme_config()`.
    let cfg = crate::acme_orchestrator::effective_acme_config(&state).await;
    if !cfg.acme_enabled {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "ACME is not enabled." }),
        )
        .await;
    }
    let domains: Vec<String> = cfg
        .acme_domains
        .split(',')
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    if domains.is_empty() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "No domains configured." }),
        )
        .await;
    }
    // The "Issue/Renew Now" button always attempts (re)issuance, even when a
    // valid certificate already exists — `force` bypasses the "already valid"
    // short-circuit that previously made the button appear to do nothing.
    // The returned object carries `issued`/`renewed` plus human-readable
    // `logs`/`error` so the UI can explain what happened.
    let result = crate::acme_orchestrator::ensure_acme_cert(&state, &cfg, true).await;
    let mut result = result;
    // Same private-key masking as the GET endpoint (ANT-275).
    crate::acme_orchestrator::mask_status_key(&mut result);
    if let Some(obj) = result.as_object_mut() {
        let issued = obj.get("issued").and_then(|v| v.as_bool()).unwrap_or(false);
        obj.insert("renewed".into(), Value::Bool(issued));
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, result).await
}

// ---------------------------------------------------------------------------
// App stores
// ---------------------------------------------------------------------------

/// `GET /api/admin/apps/stores` — the configured app stores.
async fn get_app_stores(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.stores") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let stores = state.app_stores.read().await.clone();
    let value = serde_json::to_value(&stores).unwrap_or_default();
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/apps/stores` — add an app store.
async fn add_app_store(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.stores") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let store: catalog::AppStore = match serde_json::from_value(decrypted) {
        Ok(s) => s,
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
    {
        let stores = state.app_stores.read().await;
        if stores.iter().any(|s| s.name == store.name) {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::CONFLICT,
                serde_json::json!({ "detail": format!("App store with name '{}' already exists.", store.name) }),
            )
            .await;
        }
    }
    state.app_stores.write().await.push(store.clone());
    catalog::save_app_stores(&state).await;
    let value = serde_json::to_value(&store).unwrap_or_default();
    e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await
}

/// `DELETE /api/admin/apps/stores/{store_name}` — remove an app store.
async fn delete_app_store(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(store_name): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.stores") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let mut stores = state.app_stores.write().await;
    let before = stores.len();
    stores.retain(|s| s.name != store_name);
    if stores.len() == before {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "App store not found." }),
        )
        .await;
    }
    drop(stores);
    catalog::save_app_stores(&state).await;
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await
}

// ---------------------------------------------------------------------------
// Meta apps
// ---------------------------------------------------------------------------

/// `POST /api/admin/apps/meta` — create a meta app (a customised copy of an
/// installed app with its own home template).
async fn create_meta_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: CreateMetaAppRequest = match serde_json::from_value(decrypted) {
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
    let base_app = state.installed_apps.read().await.get(&req.base_app_id).cloned();
    let Some(base_app) = base_app else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("Base application with ID '{}' not found.", req.base_app_id) }),
        )
        .await;
    };
    let new_app_id = catalog::random_uuid();
    let home_template_name = format!("meta_{new_app_id}");
    let mut logo_url = base_app.logo.clone();
    if !req.logo.is_empty() {
        match base64::engine::general_purpose::STANDARD.decode(&req.logo) {
            Ok(icon_data) => {
                let icon_path = format!("{}/{}.png", state.settings.app_icons_path, new_app_id);
                if std::fs::write(&icon_path, &icon_data).is_ok() {
                    logo_url = format!("/api/app_icon/{new_app_id}");
                    info!("Saved custom icon for new meta app {new_app_id}");
                } else {
                    logo_url = req.logo.clone();
                }
            }
            Err(_) => logo_url = req.logo.clone(),
        }
    }
    let template_dir = format!("{}/{}", state.settings.home_templates_path, home_template_name);
    let _ = std::fs::create_dir_all(&template_dir);
    let _ = std::fs::create_dir_all(format!("{template_dir}/Desktop/files"));

    let mut meta_app = base_app.clone();
    meta_app.id = new_app_id.clone();
    meta_app.name = req.name;
    meta_app.logo = logo_url;
    meta_app.provider_config.base.custom_autostart_script_b64 = req.custom_autostart_script_b64;
    meta_app.provider_config.base.custom_autostart_wayland_script_b64 = req.custom_autostart_wayland_script_b64;
    meta_app.is_meta_app = true;
    meta_app.base_app_id = Some(req.base_app_id);
    meta_app.home_template_name = Some(home_template_name);
    meta_app.users = req.users;
    meta_app.groups = req.groups;
    meta_app.auto_update = false;
    meta_app.source = base_app.name;
    meta_app.source_app_id = base_app.source_app_id;

    state.installed_apps.write().await.insert(new_app_id, meta_app.clone());
    catalog::save_installed_apps(&state).await;
    let value = serde_json::to_value(&meta_app).unwrap_or_default();
    e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await
}

/// `POST /api/admin/launch/meta_customize` — launch a meta app for
/// customisation (RW mount of its home template, no access check).
async fn launch_meta_for_customization(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: LaunchMetaCustomizeRequest = match serde_json::from_value(decrypted) {
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
    // Mirrors the Python: an unknown app and a non-meta app both map to the
    // same 400 (the endpoint is only meaningful for meta apps).
    let app_config = state.installed_apps.read().await.get(&req.application_id).cloned();
    let Some(app_config) = app_config else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "This endpoint is only for customizing meta applications." }),
        )
        .await;
    };
    if !app_config.is_meta_app {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "This endpoint is only for customizing meta applications." }),
        )
        .await;
    }
    let template_path = format!(
        "{}/{}",
        state.settings.home_templates_path,
        app_config.home_template_name.as_deref().unwrap_or("")
    );
    match catalog::launch_common(
        &state,
        &req.application_id,
        &user.record.username,
        &user.effective_settings,
        None,
        HashMap::new(),
        req.language.as_deref(),
        req.selected_gpu.as_deref(),
        None,
        None,
        false,
        Some(&template_path),
        false,
        req.wayland_mode,
        None,
        false,
    )
    .await
    {
        Ok(resp) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::to_value(&resp).unwrap_or_default()).await,
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

// ---------------------------------------------------------------------------
// Available apps (app store browsing)
// ---------------------------------------------------------------------------

/// `GET /api/admin/apps/available?url=...&store_name=...&refresh=...` — the
/// list of apps available in a store (from cache or fetched live).
async fn get_available_apps(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AvailableAppsQuery>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.stores") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let cache_file = format!("{}/{}.yml", state.settings.app_store_cache_path, q.store_name);
    // If a cache exists and we're not refreshing, try to serve it (read +
    // parse). A failure to read OR parse falls through to a fresh fetch
    // (mirrors the Python, where `process_content` sits inside the cache
    // try-block, so a poisoned cache is not sticky).
    if !q.refresh && std::path::Path::new(&cache_file).exists() {
        if let Ok(c) = std::fs::read_to_string(&cache_file) {
            match process_store_content(&state, &c, &q.store_name) {
                Ok(apps) => {
                    return e2ee::encrypt_response(&state, &headers, StatusCode::OK, apps).await
                }
                Err(_) => {
                    warn!("Cache read failed for {}: falling through to fetch", q.store_name);
                }
            }
        }
    }
    // Fresh fetch (no cache, refresh, or a poisoned cache).
    let content = match fetch_store_yaml(&state, &q.url, &q.store_name).await {
        Ok(c) => c,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d }))
                .await
        }
    };
    match process_store_content(&state, &content, &q.store_name) {
        Ok(apps) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, apps).await,
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

/// Fetch an app store's YAML from its URL and cache it. Returns the text.
async fn fetch_store_yaml(
    state: &AppState,
    url: &str,
    store_name: &str,
) -> Result<String, (StatusCode, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Failed to fetch app store from URL '{url}': {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Failed to fetch app store from URL '{url}': {e}")))?;
    if !resp.status().is_success() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Failed to fetch app store from URL '{url}' (HTTP {})", resp.status()),
        ));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Failed to read app store body: {e}")))?;
    let _ = std::fs::create_dir_all(&state.settings.app_store_cache_path);
    let cache_file = format!("{}/{}.yml", state.settings.app_store_cache_path, store_name);
    let _ = std::fs::write(&cache_file, &text);
    Ok(text)
}

/// Parse an app store's YAML into a list of available apps, attaching any
/// cached autostart scripts. Mirrors `process_content`.
fn process_store_content(
    state: &AppState,
    content: &str,
    store_name: &str,
) -> Result<Value, (StatusCode, String)> {
    let loaded: serde_yaml::Value = serde_yaml::from_str(content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse app store YAML: {e}")))?;
    let json = serde_json::to_value(loaded).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse app store YAML: {e}"))
    })?;
    let apps_list = if let Some(a) = json.get("apps").and_then(|a| a.as_array()) {
        a.clone()
    } else if let Some(a) = json.as_array() {
        a.clone()
    } else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "App store YAML has an invalid format.".to_string(),
        ));
    };
    let mut out = Vec::new();
    for mut app in apps_list {
        let provider_config = app
            .get_mut("provider_config")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let mut pc = provider_config;
        if pc.get("autostart").and_then(|v| v.as_bool()).unwrap_or(false) {
            if let Some(app_id) = app.get("id").and_then(|v| v.as_str()) {
                let cache_dir = format!("{}/{}", state.settings.autostart_cache_path, store_name);
                let cache_file_path = format!("{cache_dir}/{app_id}");
                let script_content_b64 = if std::path::Path::new(&cache_file_path).exists()
                    && std::fs::metadata(&cache_file_path).map(|m| m.len() > 0).unwrap_or(false)
                {
                    std::fs::read(&cache_file_path)
                        .ok()
                        .map(|b| base64::engine::general_purpose::STANDARD.encode(&b))
                } else {
                    None
                };
                pc["custom_autostart_script_b64"] = script_content_b64.map(Value::from).unwrap_or(Value::Null);
                let cache_file_path_wayland = format!("{cache_dir}/{app_id}-wayland");
                let wayland_script_content_b64 = if std::path::Path::new(&cache_file_path_wayland).exists()
                    && std::fs::metadata(&cache_file_path_wayland).map(|m| m.len() > 0).unwrap_or(false)
                {
                    std::fs::read(&cache_file_path_wayland)
                        .ok()
                        .map(|b| base64::engine::general_purpose::STANDARD.encode(&b))
                } else {
                    None
                };
                pc["custom_autostart_wayland_script_b64"] = wayland_script_content_b64.map(Value::from).unwrap_or(Value::Null);
            }
        }
        if let Some(ext) = pc.get("extensions").cloned() {
            if let Some(orig) = ext.as_array() {
                let mut flattened = Vec::new();
                for item in orig {
                    if let Some(inner) = item.as_array() {
                        flattened.extend(inner.iter().cloned());
                    } else {
                        flattened.push(item.clone());
                    }
                }
                pc["extensions"] = Value::Array(flattened);
            }
        }
        app["provider_config"] = pc;
        out.push(app);
    }
    Ok(Value::Array(out))
}

// ---------------------------------------------------------------------------
// Installed apps
// ---------------------------------------------------------------------------

/// `GET /api/admin/apps/installed` — the installed apps with image status.
async fn list_installed_apps(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let apps = state.installed_apps.read().await.clone();
    let metadata = state.image_metadata.read().await.clone();
    let pull_status = state.pull_status.read().await.clone();
    let pull_progress = state
        .pull_progress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut response_apps: Vec<Value> = Vec::new();
    for app in apps.values() {
        let mut app_dict = serde_json::to_value(app).unwrap_or_default();
        let image_name = app.provider_config.base.image.clone();
        let meta = metadata.get(&image_name).cloned().unwrap_or_default();
        app_dict["image_sha"] = meta.get("sha").cloned().unwrap_or(Value::Null);
        app_dict["last_checked_at"] = meta.get("last_checked_at").cloned().unwrap_or(Value::Null);
        // Update-check state from the metadata cache (ANT-269): `null` when the
        // image has never been checked against its registry. The list endpoint
        // stays cache-only — no live registry calls during a table render.
        app_dict["update_available"] = meta.get("update_available").cloned().unwrap_or(Value::Null);
        app_dict["last_update_checked_at"] = meta.get("last_update_checked_at").cloned().unwrap_or(Value::Null);
        app_dict["pull_status"] = pull_status.get(&image_name).cloned().map(Value::from).unwrap_or(Value::Null);
        if let Some(prog) = pull_progress.get(&image_name) {
            let mut clean = serde_json::Map::new();
            if let Some(obj) = prog.as_object() {
                for (k, v) in obj {
                    if !k.starts_with('_') {
                        clean.insert(k.clone(), v.clone());
                    }
                }
            }
            app_dict["pull_progress"] = Value::Object(clean);
        }
        response_apps.push(app_dict);
    }
    response_apps.sort_by(|a, b| {
        a.get("name").and_then(|n| n.as_str()).unwrap_or("")
            .to_lowercase()
            .cmp(&b.get("name").and_then(|n| n.as_str()).unwrap_or("").to_lowercase())
    });
    let value = Value::Array(response_apps);
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/apps/installed` — install an app (and queue its image pull).
async fn install_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let app: catalog::InstalledApp = match serde_json::from_value(decrypted) {
        Ok(a) => a,
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
    {
        let apps = state.installed_apps.read().await;
        if apps.contains_key(&app.id) {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::CONFLICT,
                serde_json::json!({ "detail": "App with this ID already exists." }),
            )
            .await;
        }
    }
    state.installed_apps.write().await.insert(app.id.clone(), app.clone());
    catalog::save_installed_apps(&state).await;
    let image = app.provider_config.base.image.clone();
    // Scope the read guard so it drops BEFORE we take the write lock below;
    // holding a read guard while acquiring a write lock in the same task
    // deadlocks tokio's RwLock (it is not reentrant).
    let already = {
        let pull_status = state.pull_status.read().await;
        matches!(pull_status.get(&image).map(|s| s.as_str()), Some("queued") | Some("pulling"))
    };
    if !already {
        state.pull_status.write().await.insert(image.clone(), "queued".to_string());
        let state_clone = state.clone();
        let app_clone = app.clone();
        tokio::spawn(async move {
            crate::catalog::do_pull_and_cache_image(&state_clone, &image, Some(app_clone)).await;
        });
    }
    let value = serde_json::to_value(&app).unwrap_or_default();
    e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await
}

/// `PUT /api/admin/apps/installed/{app_id}` — update an installed app.
async fn update_installed_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(app_id): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let mut app_update: catalog::InstalledApp = match serde_json::from_value(decrypted) {
        Ok(a) => a,
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
    {
        let apps = state.installed_apps.read().await;
        if !apps.contains_key(&app_id) {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::NOT_FOUND,
                serde_json::json!({ "detail": "Installed app not found." }),
            )
            .await;
        }
    }
    if app_id != app_update.id {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "App ID in path does not match body." }),
        )
        .await;
    }
    if !app_update.logo.is_empty() && !app_update.logo.starts_with("/api/app_icon/") {
        match base64::engine::general_purpose::STANDARD.decode(&app_update.logo) {
            Ok(icon_data) => {
                let icon_path = format!("{}/{}.png", state.settings.app_icons_path, app_id);
                if std::fs::write(&icon_path, &icon_data).is_ok() {
                    app_update.logo = format!("/api/app_icon/{app_id}");
                    info!("Updated and saved custom icon for app {app_id}");
                } else {
                    warn!("Could not write logo for app {app_id}; leaving as URL.");
                }
            }
            Err(_) => warn!("Could not decode logo for app {app_id}. Assuming it's a URL and leaving as is."),
        }
    }
    if app_update.provider_config.base.custom_autostart_script_b64.as_deref() == Some("") {
        app_update.provider_config.base.custom_autostart_script_b64 = None;
    }
    if app_update.provider_config.base.custom_autostart_wayland_script_b64.as_deref() == Some("") {
        app_update.provider_config.base.custom_autostart_wayland_script_b64 = None;
    }
    let old_image_name = state
        .installed_apps
        .read()
        .await
        .get(&app_id)
        .map(|a| a.provider_config.base.image.clone())
        .unwrap_or_default();
    state.installed_apps.write().await.insert(app_id.clone(), app_update.clone());
    catalog::save_installed_apps(&state).await;
    let new_image = app_update.provider_config.base.image.clone();
    if old_image_name != new_image {
        let pull_status = state.pull_status.read().await;
        let already = matches!(pull_status.get(&new_image).map(|s| s.as_str()), Some("queued") | Some("pulling"));
        if !already {
            state.pull_status.write().await.insert(new_image.clone(), "queued".to_string());
            let state_clone = state.clone();
            let app_clone = app_update.clone();
            tokio::spawn(async move {
                crate::catalog::do_pull_and_cache_image(&state_clone, &new_image, Some(app_clone)).await;
            });
        }
    }
    let value = serde_json::to_value(&app_update).unwrap_or_default();
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `DELETE /api/admin/apps/installed/{app_id}` — delete an installed app.
async fn delete_installed_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(app_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let app_to_delete = state.installed_apps.read().await.get(&app_id).cloned();
    let Some(app_to_delete) = app_to_delete else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Installed app not found." }),
        )
        .await;
    };
    if app_to_delete.is_meta_app {
        let icon_path = format!("{}/{}.png", state.settings.app_icons_path, app_id);
        if std::path::Path::new(&icon_path).exists() {
            let _ = std::fs::remove_file(&icon_path);
        }
        if let Some(template_name) = &app_to_delete.home_template_name {
            let template_dir_path = format!("{}/{}", state.settings.home_templates_path, template_name);
            if std::path::Path::new(&template_dir_path).is_dir() {
                let _ = std::fs::remove_dir_all(&template_dir_path);
                info!("Purged home template directory for meta-app {app_id}: {template_dir_path}");
            }
        }
    }
    state.installed_apps.write().await.remove(&app_id);
    catalog::save_installed_apps(&state).await;
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await
}

/// `POST /api/admin/apps/installed/{app_id}/check_update` — check whether a
/// newer image is available for an app.
async fn check_app_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(app_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let app = state.installed_apps.read().await.get(&app_id).cloned();
    let Some(app) = app else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Installed app not found." }),
        )
        .await;
    };
    let image_name = app.provider_config.base.image.clone();
    // Test hook: when `fake_provider` is set, use a deterministic fake digest
    // (mirrors the Python `FakeProvider` monkeypatch) instead of the daemon.
    let (local_info, remote_digest) = if state
        .fake_provider
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        let local = serde_json::json!({
            "id": "sha256:local",
            "short_id": "local123",
            "digests": ["sha256:local"],
        });
        let remote = if image_name == "lscr.io/linuxserver/firefox:latest" {
            "sha256:local".to_string()
        } else {
            "sha256:remote".to_string()
        };
        (Some(local), Some(remote))
    } else {
        let provider = match DockerProvider::new(serde_json::to_value(&app).unwrap_or_default()) {
            Ok(p) => p,
            Err(e) => {
                return e2ee::encrypt_response(
                    &state,
                    &headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "detail": e.detail }),
                )
                .await
            }
        };
        (
            provider.get_local_image_info(&image_name).await,
            provider.get_remote_image_digest(&image_name).await,
        )
    };
    let Some(remote_digest) = remote_digest else {
        // The registry lookup failed, but a successful local daemon query
        // still refreshes the table's sha/digests (ANT-269).
        crate::catalog::persist_image_metadata(&state, &image_name, local_info.as_ref(), None).await;
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_GATEWAY,
            serde_json::json!({ "detail": format!("Could not retrieve update information for {image_name} from its registry.") }),
        )
        .await;
    };
    let local_digests = local_info
        .as_ref()
        .and_then(|i| i.get("digests").and_then(|d| d.as_array()))
        .cloned()
        .unwrap_or_default();
    let update_available = !local_digests.iter().any(|d| d.as_str().map(|s| s.contains(&remote_digest)).unwrap_or(false));
    // Persist the refreshed metadata (sha/digests) plus the update-check
    // state, so the installed-apps table reflects the Check result
    // (ANT-269).
    crate::catalog::persist_image_metadata(
        &state,
        &image_name,
        local_info.as_ref(),
        Some((remote_digest.as_str(), update_available)),
    )
    .await;
    let current_sha = local_info
        .as_ref()
        .and_then(|i| i.get("short_id").and_then(|s| s.as_str()))
        .map(|s| s.to_string());
    let value = serde_json::json!({
        "current_sha": current_sha,
        "update_available": update_available,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/apps/installed/check_all_updates` — check every installed
/// app's image against its registry (in parallel).
async fn check_all_app_updates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let apps = state.installed_apps.read().await.values().cloned().collect::<Vec<_>>();
    let mut results: Vec<Value> = Vec::new();
    let mut updates_available = 0;
    for app in apps {
        let image_name = app.provider_config.base.image.clone();
        // Test hook: when `fake_provider` is set, use a deterministic fake
        // digest (mirrors the Python `FakeProvider` monkeypatch).
        let (local_info, remote_digest) = if state
            .fake_provider
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let local = serde_json::json!({
                "id": "sha256:local",
                "short_id": "local123",
                "digests": ["sha256:local"],
            });
            let remote = if image_name == "lscr.io/linuxserver/firefox:latest" {
                "sha256:local".to_string()
            } else {
                "sha256:remote".to_string()
            };
            (Some(local), Some(remote))
        } else {
            let provider = match DockerProvider::new(serde_json::to_value(&app).unwrap_or_default()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            (
                provider.get_local_image_info(&image_name).await,
                provider.get_remote_image_digest(&image_name).await,
            )
        };
        let local_digests = local_info
            .as_ref()
            .and_then(|i| i.get("digests").and_then(|d| d.as_array()))
            .cloned()
            .unwrap_or_default();
        let update_available = remote_digest
            .as_ref()
            .map(|rd| !local_digests.iter().any(|d| d.as_str().map(|s| s.contains(rd)).unwrap_or(false)))
            .unwrap_or(false);
        if update_available {
            updates_available += 1;
        }
        // Persist the refreshed metadata (sha/digests) plus the update-check
        // state (only when the registry comparison succeeded), so the
        // installed-apps table reflects the Check result (ANT-269).
        crate::catalog::persist_image_metadata(
            &state,
            &image_name,
            local_info.as_ref(),
            remote_digest.as_deref().map(|rd| (rd, update_available)),
        )
        .await;
        let current_sha = local_info
            .as_ref()
            .and_then(|i| i.get("short_id").and_then(|s| s.as_str()))
            .map(|s| s.to_string());
        results.push(serde_json::json!({
            "app_id": app.id,
            "name": app.name,
            "image": image_name,
            "current_sha": current_sha,
            "update_available": update_available,
        }));
    }
    let value = serde_json::json!({
        "results": results,
        "updates_available": updates_available,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/apps/installed/pull_all_latest` — kick off a background
/// pull of the latest image for every installed app.
async fn pull_all_app_images(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let apps = state.installed_apps.read().await.values().cloned().collect::<Vec<_>>();
    let mut results: Vec<Value> = Vec::new();
    for app in apps {
        let image_name = app.provider_config.base.image.clone();
        let already = matches!(
            state.pull_status.read().await.get(&image_name).map(|s| s.as_str()),
            Some("queued") | Some("pulling")
        );
        if !already {
            state.pull_status.write().await.insert(image_name.clone(), "queued".to_string());
            let state_clone = state.clone();
            let app_clone = app.clone();
            let image_clone = image_name.clone();
            tokio::spawn(async move {
                crate::catalog::do_pull_and_cache_image(&state_clone, &image_clone, Some(app_clone)).await;
            });
        }
        results.push(serde_json::json!({
            "app_id": app.id,
            "name": app.name,
            "image": image_name,
            "status": "pulling",
        }));
    }
    let value = serde_json::json!({
        "results": results,
        "started": results.len(),
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/apps/installed/{app_id}/pull_latest` — kick off a
/// background pull of the latest image for one app.
async fn pull_latest_app_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(app_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let app = state.installed_apps.read().await.get(&app_id).cloned();
    let Some(app) = app else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Installed app not found." }),
        )
        .await;
    };
    let image_name = app.provider_config.base.image.clone();
    let already = matches!(
        state.pull_status.read().await.get(&image_name).map(|s| s.as_str()),
        Some("queued") | Some("pulling")
    );
    if already {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::OK,
            serde_json::json!({ "status": "pulling", "new_sha": null }),
        )
        .await;
    }
    state.pull_status.write().await.insert(image_name.clone(), "queued".to_string());
    let state_clone = state.clone();
    let app_clone = app.clone();
    tokio::spawn(async move {
        crate::catalog::do_pull_and_cache_image(&state_clone, &image_name, Some(app_clone)).await;
    });
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::json!({ "status": "pulling", "new_sha": null }),
    )
    .await
}

/// `GET /api/admin/apps/auto_update_status` — read-only bookkeeping for the
/// background auto-update job (ANT-269): the effective settings (the DB
/// override wins over the environment), the last/next tick times from the
/// job (both `null` before the first tick completes, e.g. right after a
/// restart), how many installed apps currently have an update available per
/// the metadata cache, and how many installed apps have opted into
/// auto-update.
async fn get_auto_update_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.apps") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let enabled = state
        .settings
        .get_setting(&state.db, "auto_update_apps")
        .await
        .as_bool()
        .unwrap_or(true);
    let interval = state
        .settings
        .get_setting(&state.db, "auto_update_interval_seconds")
        .await
        .as_i64()
        .unwrap_or(3600);
    let job = state.auto_update_job.read().await.clone();
    let apps = state.installed_apps.read().await.values().cloned().collect::<Vec<_>>();
    let metadata = state.image_metadata.read().await.clone();
    let mut updates_available = 0;
    let mut auto_update_apps_count = 0;
    for app in &apps {
        if app.auto_update {
            auto_update_apps_count += 1;
        }
        let meta = metadata.get(&app.provider_config.base.image);
        if meta
            .and_then(|m| m.get("update_available"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            updates_available += 1;
        }
    }
    let value = serde_json::json!({
        "enabled": enabled,
        "interval_seconds": interval,
        "last_check_at": job.last_check_at.map(Value::from),
        "next_check_at": job.next_check_at.map(Value::from),
        "updates_available": updates_available,
        "auto_update_apps_count": auto_update_apps_count,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

// ---------------------------------------------------------------------------
// App templates + template schema
// ---------------------------------------------------------------------------

/// `GET /api/admin/apps/templates` — the application templates, sorted by name.
async fn get_app_templates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.templates") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let templates = state.app_templates.read().await.clone();
    let mut list: Vec<Value> = templates.values().cloned().collect();
    list.sort_by(|a, b| {
        a.get("name").and_then(|n| n.as_str()).unwrap_or("")
            .cmp(b.get("name").and_then(|n| n.as_str()).unwrap_or(""))
    });
    let value = Value::Array(list);
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `GET /api/admin/apps/templates/schema` — the categorised template schema.
async fn get_template_schema(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.templates") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let schema = state.template_schema.read().await.clone();
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, schema).await
}

/// `POST /api/admin/apps/templates/schema` — save the template schema (the
/// base layer). Replaces the whole schema: upsert by name, remove missing,
/// re-order.
async fn save_template_schema(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.templates") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let body = if decrypted.is_null() { Value::Object(serde_json::Map::new()) } else { decrypted };
    let Some(settings_list) = body.get("settings") else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "'settings' is required." }),
        )
        .await;
    };
    let Some(settings_list) = settings_list.as_array() else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "'settings' must be a list." }),
        )
        .await;
    };
    // The bundled YAML baseline (for built-in settings' read-only default).
    let yaml_by_name: HashMap<String, Value> = catalog::load_template_schema_yaml(&state.settings)
        .into_iter()
        .filter_map(|s| {
            let name = s.get("name").and_then(|n| n.as_str())?.to_string();
            Some((name, s))
        })
        .collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut clean: Vec<(i32, Value)> = Vec::new();
    for (i, s) in settings_list.iter().enumerate() {
        let Some(s_obj) = s.as_object() else {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "detail": "Each setting must be an object." }),
            )
            .await
        };
        let name = s_obj.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if !name.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "detail": format!("Invalid setting name: '{name}'.") }),
            )
            .await;
        }
        if !seen.insert(name.clone()) {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "detail": format!("Duplicate setting name: '{name}'.") }),
            )
            .await;
        }
        let stype = s_obj.get("type").and_then(|v| v.as_str()).unwrap_or("text");
        if !matches!(stype, "text" | "boolean" | "select") {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "detail": format!("Invalid type '{stype}' for setting '{name}'.") }),
            )
            .await;
        }
        // The baseline always tracks the bundled YAML for built-in settings.
        let default_val = if let Some(y) = yaml_by_name.get(&name) {
            y.get("default").and_then(|v| v.as_str()).unwrap_or("").to_string()
        } else {
            s_obj.get("default").and_then(|v| v.as_str()).unwrap_or("").to_string()
        };
        let raw_current = s_obj.get("current");
        let current_val = match raw_current {
            None => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(v) => Some(v.as_str().unwrap_or("").to_string()),
        };
        let mut entry = serde_json::json!({
            "name": name,
            "label": s_obj.get("label").and_then(|v| v.as_str()).unwrap_or(&name),
            "description": s_obj.get("description").and_then(|v| v.as_str()).unwrap_or(""),
            "category": s_obj.get("category").and_then(|v| v.as_str()).unwrap_or("general"),
            "type": stype,
            "default": default_val,
            "current": current_val,
            "docker": s_obj.get("docker").and_then(|v| v.as_bool()).unwrap_or(false),
        });
        if stype == "select" {
            // Mirrors the Python filter: keep only dict entries whose
            // `value` is present and not None (drops non-dict entries and
            // `{"value": null}`).
            let options = s_obj
                .get("options")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|o| o.get("value").map(|v| !v.is_null()).unwrap_or(false))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            entry["options"] = Value::Array(options);
        }
        clean.push((i as i32, entry));
    }
    // Persist: delete missing, upsert the rest in order.
    let conn = &state.db.conn;
    let existing = match entities::template_schema_setting::Entity::find().all(conn).await {
        Ok(rows) => rows,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": format!("Database error: {e}") }),
            )
            .await
        }
    };
    let existing_names: std::collections::HashSet<String> =
        existing.iter().map(|r| r.name.clone()).collect();
    for row in &existing {
        if !seen.contains(&row.name) {
            let am = entities::template_schema_setting::ActiveModel {
                name: ActiveValue::Set(row.name.clone()),
                sort_order: ActiveValue::NotSet,
                label: ActiveValue::NotSet,
                description: ActiveValue::NotSet,
                category: ActiveValue::NotSet,
                type_: ActiveValue::NotSet,
                default: ActiveValue::NotSet,
                current: ActiveValue::NotSet,
                docker: ActiveValue::NotSet,
                options: ActiveValue::NotSet,
            };
            if db_fails_writes(&state)
                || entities::template_schema_setting::Entity::delete(am).exec(conn).await.is_err()
            {
                return e2ee::encrypt_response(
                    &state,
                    &headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "detail": format!("Database error deleting template setting '{}'.", row.name) }),
                )
                .await;
            }
        }
    }
    for (i, entry) in clean.iter() {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let exists = existing_names.contains(name);
        let am = entities::template_schema_setting::ActiveModel {
            name: ActiveValue::Set(name.to_string()),
            sort_order: ActiveValue::Set(*i),
            label: ActiveValue::Set(
                entry.get("label").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            ),
            description: ActiveValue::Set(
                entry.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            ),
            category: ActiveValue::Set(
                entry.get("category").and_then(|v| v.as_str()).unwrap_or("general").to_string(),
            ),
            type_: ActiveValue::Set(
                entry.get("type").and_then(|v| v.as_str()).unwrap_or("text").to_string(),
            ),
            default: ActiveValue::Set(
                entry.get("default").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            ),
            current: ActiveValue::Set(
                entry.get("current").and_then(|v| v.as_str()).map(|s| s.to_string()),
            ),
            docker: ActiveValue::Set(
                entry.get("docker").and_then(|v| v.as_bool()).unwrap_or(false),
            ),
            options: ActiveValue::Set(
                entry.get("options").cloned().unwrap_or(serde_json::json!([])),
            ),
        };
        if exists {
            if db_fails_writes(&state)
                || entities::template_schema_setting::Entity::update(am).exec(conn).await.is_err()
            {
                return e2ee::encrypt_response(
                    &state,
                    &headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "detail": format!("Database error updating template setting '{name}'.") }),
                )
                .await;
            }
        } else if db_fails_writes(&state)
            || entities::template_schema_setting::Entity::insert(am).exec(conn).await.is_err()
        {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": format!("Database error inserting template setting '{name}'.") }),
            )
            .await;
        }
    }
    // Rebuild the in-memory schema so the launch path sees the change.
    catalog::load_template_schema(&state).await;
    info!("Template schema updated ({} settings).", clean.len());
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::json!({ "ok": true })).await
}

/// `POST /api/admin/apps/templates` — create or override an app template.
async fn save_app_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.templates") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    // Mirrors the Pydantic `AppTemplate` model: `name` is a required field,
    // so a missing `name` is a 422 validation error (the detail names the
    // missing field); a present-but-invalid `name` is a 400.
    let Some(name) = decrypted.get("name").and_then(|v| v.as_str()) else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "Invalid request body: 'name' is required." }),
        )
        .await;
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-'))
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "Invalid template name." }),
        )
        .await;
    }
    let settings = decrypted.get("settings").cloned().unwrap_or(serde_json::json!({}));
    let template = serde_json::json!({ "name": name, "settings": settings });
    state.app_templates.write().await.insert(name.to_string(), template.clone());
    let conn = &state.db.conn;
    let existing = entities::app_template::Entity::find()
        .filter(entities::app_template::Column::Name.eq(name))
        .one(conn)
        .await
        .ok()
        .flatten();
    if let Some(row) = existing {
        let am = entities::app_template::ActiveModel {
            name: ActiveValue::Set(row.name),
            settings: ActiveValue::Set(settings),
        };
        let _ = entities::app_template::Entity::update(am).exec(conn).await;
    } else {
        let am = entities::app_template::ActiveModel {
            name: ActiveValue::Set(name.to_string()),
            settings: ActiveValue::Set(settings),
        };
        let _ = entities::app_template::Entity::insert(am).exec(conn).await;
    }
    let value = template;
    e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await
}

/// `DELETE /api/admin/apps/templates/{template_name}` — delete a user template.
async fn delete_app_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(template_name): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.templates") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let default_filename = format!("{}.yml", template_name.to_lowercase().replace(' ', "_"));
    let default_path = format!("{}/{}", state.settings.default_app_templates_path, default_filename);
    let is_default = std::path::Path::new(&default_path).exists();
    let conn = &state.db.conn;
    let user_row = entities::app_template::Entity::find()
        .filter(entities::app_template::Column::Name.eq(&template_name))
        .one(conn)
        .await
        .ok()
        .flatten();
    if user_row.is_none() && is_default {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": format!("Cannot delete the default template '{template_name}'. You can override it by creating a new template with the same name.") }),
        )
        .await;
    }
    if user_row.is_none() && !state.app_templates.read().await.contains_key(&template_name) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("Template '{template_name}' not found.") }),
        )
        .await;
    }
    state.app_templates.write().await.remove(&template_name);
    if is_default {
        if let Ok(text) = std::fs::read_to_string(&default_path) {
            if let Ok(yaml_val) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
                if let Ok(data) = serde_json::to_value(yaml_val) {
                    if data.get("name").is_some() {
                        state
                            .app_templates
                            .write()
                            .await
                            .insert(template_name.clone(), data);
                    }
                }
            }
        }
    }
    if let Some(row) = user_row {
        let am = entities::app_template::ActiveModel {
            name: ActiveValue::Set(row.name),
            settings: ActiveValue::NotSet,
        };
        let _ = entities::app_template::Entity::delete(am).exec(conn).await;
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await
}

// ---------------------------------------------------------------------------
// Session oversight
// ---------------------------------------------------------------------------

/// `GET /api/admin/sessions` — every user's active sessions, grouped by user.
async fn get_all_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.sessions") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let sessions = state.sessions.read().await.clone();
    let mut by_user: HashMap<String, Vec<Value>> = HashMap::new();
    for (sid, s_data) in sessions.iter() {
        let username = s_data
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let out_of_date = catalog::session_out_of_date(&state, s_data).await;
        let is_collaboration = s_data
            .get("is_collaboration")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let session_url = if is_collaboration {
            let token = s_data
                .get("controller_token")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            format!("/room/{sid}?token={token}")
        } else {
            let token = s_data
                .get("access_token")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            format!("/api/apps/session/{sid}/?access_token={token}")
        };
        by_user
            .entry(username)
            .or_default()
            .push(serde_json::json!({
                "session_id": sid,
                "app_id": s_data.get("provider_app_id").and_then(|v| v.as_str()).unwrap_or_default(),
                "app_name": s_data.get("app_name").and_then(|v| v.as_str()).unwrap_or_default(),
                "app_logo": s_data.get("app_logo").and_then(|v| v.as_str()).unwrap_or_default(),
                "created_at": s_data.get("created_at").and_then(|v| v.as_f64()).unwrap_or(0.0),
                "session_url": session_url,
                "launch_context": s_data.get("launch_context").cloned().unwrap_or(Value::Null),
                "is_collaboration": is_collaboration,
                "name": s_data.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()),
                "out_of_date": out_of_date,
            }));
    }
    let mut response: Vec<Value> = Vec::new();
    for (uname, mut slist) in by_user.into_iter() {
        slist.sort_by(|a, b| {
            b.get("created_at")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                .partial_cmp(&a.get("created_at").and_then(|v| v.as_f64()).unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        response.push(serde_json::json!({ "username": uname, "sessions": slist }));
    }
    response.sort_by(|a, b| {
        a.get("username").and_then(|v| v.as_str()).unwrap_or("")
            .cmp(b.get("username").and_then(|v| v.as_str()).unwrap_or(""))
    });
    let value = Value::Array(response);
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `DELETE /api/admin/sessions/{session_id}` — stop any user's session.
async fn stop_any_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(session_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.sessions") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    if !state.sessions.read().await.contains_key(&session_id) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found." }),
        )
        .await;
    }
    catalog::stop_session(&state, &session_id).await;
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await
}

/// `POST /api/admin/sessions/{session_id}/recreate` — recreate any user's
/// session from the current image.
async fn recreate_any_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(session_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.sessions") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    if !state.sessions.read().await.contains_key(&session_id) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found." }),
        )
        .await;
    }
    match catalog::recreate_session(&state, &session_id).await {
        Ok(session_data) => {
            let is_collaboration = session_data
                .get("is_collaboration")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let session_url = if is_collaboration {
                let token = session_data
                    .get("controller_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                format!("/room/{session_id}?token={token}")
            } else {
                let token = session_data
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                format!("/api/apps/session/{session_id}/?access_token={token}")
            };
            let value = serde_json::json!({ "session_id": session_id, "session_url": session_url });
            e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
        }
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

/// `POST /api/admin/oidc/revoke` — force-invalidate all web (OIDC) sessions
/// for a named user.
async fn revoke_oidc_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.sessions") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: RevokeOidcSessionRequest = match serde_json::from_value(decrypted) {
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
    if user_manager::get_user(&state.db, &req.username).await.unwrap_or(None).is_none() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("User '{}' not found.", req.username) }),
        )
        .await;
    }
    let revoked = auth::revoke_all_tokens_for_user(&state, &req.username).await;
    let _ = auth::save_auth_tokens(&state).await;
    info!("Admin force-logout: revoked {revoked} web token(s) for '{}'.", req.username);
    let value = serde_json::json!({ "username": req.username, "revoked": revoked });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

// ---------------------------------------------------------------------------
// People (accounts)
// ---------------------------------------------------------------------------

/// `POST /api/admin/people` — create a user or admin.
async fn create_person(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.accounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: CreatePersonRequest = match serde_json::from_value(decrypted) {
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
    let settings_ref = req.settings.as_ref();
    let result = if req.is_admin {
        user_manager::create_admin(&state.db, &req.username, settings_ref).await
    } else {
        user_manager::create_user(&state.db, &req.username, settings_ref).await
    };
    match result {
        Ok(created) => {
            let value = serde_json::json!({ "user": roster_entry(&created) });
            e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await
        }
        Err(e) => {
            let status = if e.status() == 404 {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::CONFLICT
            };
            e2ee::encrypt_response(&state, &headers, status, serde_json::json!({ "detail": e.to_string() })).await
        }
    }
}

/// `PUT /api/admin/people/{username}` — update a user's settings.
async fn update_person(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(username): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.accounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: UpdateUserRequest = match serde_json::from_value(decrypted) {
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
    match user_manager::update_user_settings(&state.db, &username, req.settings).await {
        Ok(()) => match user_manager::get_user(&state.db, &username).await {
            Ok(Some(u)) => {
                let value = roster_entry(&u);
                e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
            }
            Ok(None) => e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::NOT_FOUND,
                serde_json::json!({ "detail": format!("User or admin '{username}' not found.") }),
            )
            .await,
            Err(e) => e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                serde_json::json!({ "detail": e.to_string() }),
            )
            .await,
        },
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::from_u16(e.status()).unwrap_or(StatusCode::NOT_FOUND),
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

/// `POST /api/admin/people/{username}/admin-status` — promote/demote.
async fn set_admin_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(username): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.accounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: SetAdminStatusRequest = match serde_json::from_value(decrypted) {
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
    let result = if req.is_admin {
        user_manager::promote_user_to_admin(&state.db, &username).await
    } else {
        user_manager::demote_admin_to_user(&state.db, &state.settings, &username).await
    };
    match result {
        Ok(()) => match user_manager::get_user(&state.db, &username).await {
            Ok(Some(u)) => {
                let value = roster_entry(&u);
                e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
            }
            Ok(None) => e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::NOT_FOUND,
                serde_json::json!({ "detail": format!("User or admin '{username}' not found.") }),
            )
            .await,
            Err(e) => e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                serde_json::json!({ "detail": e.to_string() }),
            )
            .await,
        },
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.to_lowercase().contains("cannot be demoted") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::NOT_FOUND)
            };
            e2ee::encrypt_response(&state, &headers, status, serde_json::json!({ "detail": msg })).await
        }
    }
}

/// `DELETE /api/admin/people/{username}` — delete a user or admin.
async fn delete_person(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(username): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.accounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    match user_manager::delete_person(&state.db, &state.settings, &username).await {
        Ok(()) => e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await,
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.to_lowercase().contains("cannot be deleted") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::NOT_FOUND)
            };
            e2ee::encrypt_response(&state, &headers, status, serde_json::json!({ "detail": msg })).await
        }
    }
}

/// `GET /api/admin/people/{username}/homedirs` — list a user's home dirs.
async fn list_person_home_dirs(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(username): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.accounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    if user_manager::get_user(&state.db, &username).await.unwrap_or(None).is_none() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("User or admin '{username}' not found.") }),
        )
        .await;
    }
    let effective = user_manager::get_effective_settings(&state.db, &username).await.unwrap_or_default();
    if !effective.get("persistent_storage").and_then(|v| v.as_bool()).unwrap_or(false) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Persistent storage is disabled for this account." }),
        )
        .await;
    }
    let home_dirs = user_manager::get_home_dirs(&state.settings, &username);
    let value = serde_json::json!({ "home_dirs": home_dirs });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/people/{username}/homedirs` — create a home dir.
async fn create_person_home_dir(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(username): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.accounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    if user_manager::get_user(&state.db, &username).await.unwrap_or(None).is_none() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("User or admin '{username}' not found.") }),
        )
        .await;
    }
    let effective = user_manager::get_effective_settings(&state.db, &username).await.unwrap_or_default();
    if !effective.get("persistent_storage").and_then(|v| v.as_bool()).unwrap_or(false) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Persistent storage is disabled for this account." }),
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
    let req: HomeDirectoryCreate = match serde_json::from_value(decrypted) {
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
    // The home-name pattern is enforced here (mirrors the Pydantic
    // `HomeDirectoryCreate.home_name` pattern) so a malformed name is a 422,
    // not a 409 from the duplicate check in `create_home_dir`.
    if !user_manager::is_valid_home_name(&req.home_name) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "Invalid request body: home name must match pattern." }),
        )
        .await;
    }
    match user_manager::create_home_dir(&state.settings, &username, &req.home_name) {
        Ok(()) => e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, serde_json::json!({ "status": "success" })).await,
        // Mirrors Python: any ValueError from create_home_dir (duplicate, or a
        // filesystem failure) is a 409. The invalid-name case is already a 422
        // above.
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::CONFLICT,
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

/// `DELETE /api/admin/people/{username}/homedirs/{home_name}` — delete a home dir.
async fn delete_person_home_dir(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam((username, home_name)): PathParam<(String, String)>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.accounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    if user_manager::get_user(&state.db, &username).await.unwrap_or(None).is_none() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("User or admin '{username}' not found.") }),
        )
        .await;
    }
    let effective = user_manager::get_effective_settings(&state.db, &username).await.unwrap_or_default();
    if !effective.get("persistent_storage").and_then(|v| v.as_bool()).unwrap_or(false) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Persistent storage is disabled for this account." }),
        )
        .await;
    }
    match user_manager::delete_home_dir(&state.settings, &username, &home_name) {
        Ok(()) => e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await,
        // Mirrors Python: any ValueError from delete_home_dir (missing dir, or
        // a filesystem failure) is a 404.
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

/// `POST /api/admin/people/{username}/access` — set a user's roles/perms/groups.
async fn set_user_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(username): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.accounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: SetUserAccessRequest = match serde_json::from_value(decrypted) {
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
    match user_manager::set_user_access(
        &state.db,
        &state.settings,
        &username,
        req.roles,
        req.permissions,
        req.groups,
    )
    .await
    {
        Ok(updated) => {
            let value = roster_entry(&updated);
            e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
        }
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("must keep the 'admin' role") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::NOT_FOUND)
            };
            e2ee::encrypt_response(&state, &headers, status, serde_json::json!({ "detail": msg })).await
        }
    }
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// `POST /api/admin/groups` — create a group.
async fn create_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.groups") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: CreateGroupRequest = match serde_json::from_value(decrypted) {
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
    // The group-name pattern is enforced here (mirrors the Pydantic
    // `CreateGroupRequest.name` pattern) so a malformed name is a 422, not a
    // 409 from the duplicate check below.
    if !user_manager::is_valid_group_name(&req.name) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "Invalid request body: group name must match pattern." }),
        )
        .await;
    }
    if user_manager::get_group(&state.db, &req.name).await.unwrap_or(None).is_some() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::CONFLICT,
            serde_json::json!({ "detail": format!("Group '{}' already exists.", req.name) }),
        )
        .await;
    }
    match user_manager::write_group_file(
        &state.db,
        &req.name,
        req.settings,
        Some(req.roles),
        Some(req.permissions),
    )
    .await
    {
        Ok(()) => match user_manager::get_group(&state.db, &req.name).await {
            Ok(Some(g)) => {
                let value = serde_json::to_value(&g).unwrap_or_default();
                e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await
            }
            Ok(None) => e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::NOT_FOUND,
                serde_json::json!({ "detail": format!("Group '{}' not found.", req.name) }),
            )
            .await,
            Err(e) => e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                serde_json::json!({ "detail": e.to_string() }),
            )
            .await,
        },
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

/// `PUT /api/admin/groups/{group_name}` — update a group.
async fn update_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(group_name): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.groups") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: UpdateGroupRequest = match serde_json::from_value(decrypted) {
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
    let existing = user_manager::get_group(&state.db, &group_name).await.unwrap_or(None);
    let Some(existing) = existing else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("Group '{group_name}' not found.") }),
        )
        .await;
    };
    let new_settings = req
        .settings
        .clone()
        .unwrap_or_else(|| existing.settings.clone());
    let new_roles = req.roles.clone().unwrap_or_else(|| existing.roles.clone());
    let new_perms = req
        .permissions
        .clone()
        .unwrap_or_else(|| existing.permissions.clone());
    match user_manager::write_group_file(
        &state.db,
        &group_name,
        new_settings,
        Some(new_roles),
        Some(new_perms),
    )
    .await
    {
        Ok(()) => {
            // Re-derive is_admin for every member (a group change affects them).
            let users = user_manager::get_all_users(&state.db).await.unwrap_or_default();
            for u in users.iter() {
                if u.groups.iter().any(|g| g == &group_name) {
                    let _ = user_manager::recompute_is_admin(&state.db, &u.username).await;
                }
            }
            match user_manager::get_group(&state.db, &group_name).await {
                Ok(Some(g)) => {
                    let value = serde_json::to_value(&g).unwrap_or_default();
                    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
                }
                Ok(None) => e2ee::encrypt_response(
                    &state,
                    &headers,
                    StatusCode::NOT_FOUND,
                    serde_json::json!({ "detail": format!("Group '{group_name}' not found.") }),
                )
                .await,
                Err(e) => e2ee::encrypt_response(
                    &state,
                    &headers,
                    StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    serde_json::json!({ "detail": e.to_string() }),
                )
                .await,
            }
        }
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

/// `DELETE /api/admin/groups/{group_name}` — delete a group.
async fn delete_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(group_name): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.groups") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    match user_manager::delete_group(&state.db, &group_name).await {
        Ok(()) => e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await,
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::from_u16(e.status()).unwrap_or(StatusCode::NOT_FOUND),
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

// ---------------------------------------------------------------------------
// Volume mounts
// ---------------------------------------------------------------------------

/// Read all volume mounts from the database as a JSON array.
async fn list_volume_mounts_value(state: &AppState) -> Value {
    let rows = match entities::volume_mount::Entity::find().all(&state.db.conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to list volume mounts: {e}");
            return Value::Array(vec![]);
        }
    };
    let mut out = Vec::new();
    for m in rows {
        out.push(serde_json::json!({
            "name": m.name,
            "host_path": m.host_path,
            "container_path": m.container_path,
            "read_only": m.read_only,
            "scope": m.scope,
            "target": m.target,
        }));
    }
    Value::Array(out)
}

const VALID_SCOPES: &[&str] = &["none", "user", "group"];

/// `GET /api/admin/volume_mounts` — list all volume mounts.
async fn list_volume_mounts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.mounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let value = list_volume_mounts_value(&state).await;
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/volume_mounts` — create a volume mount.
async fn create_volume_mount(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.mounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: CreateVolumeMountRequest = match serde_json::from_value(decrypted) {
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
    // Mirrors the Pydantic `pattern=r"^[a-zA-Z0-9_-]+$"` on `name`.
    if !req
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || req.name.is_empty()
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "Invalid request body: name must match ^[a-zA-Z0-9_-]+$." }),
        )
        .await;
    }
    if !VALID_SCOPES.contains(&req.scope.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "scope must be 'none', 'user', or 'group'." }),
        )
        .await;
    }
    if req.scope == "user" && user_manager::get_user(&state.db, &req.target).await.unwrap_or(None).is_none() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("User '{}' not found.", req.target) }),
        )
        .await;
    }
    if req.scope == "group" && user_manager::get_group(&state.db, &req.target).await.unwrap_or(None).is_none() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("Group '{}' not found.", req.target) }),
        )
        .await;
    }
    let existing = entities::volume_mount::Entity::find_by_id(req.name.clone())
        .one(&state.db.conn)
        .await
        .ok()
        .flatten();
    if existing.is_some() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::CONFLICT,
            serde_json::json!({ "detail": format!("Volume mount '{}' already exists.", req.name) }),
        )
        .await;
    }
    let am = entities::volume_mount::ActiveModel {
        name: ActiveValue::Set(req.name.clone()),
        host_path: ActiveValue::Set(req.host_path.clone()),
        container_path: ActiveValue::Set(req.container_path.clone()),
        read_only: ActiveValue::Set(req.read_only),
        scope: ActiveValue::Set(req.scope.clone()),
        target: ActiveValue::Set(req.target.clone()),
    };
    if let Err(e) = entities::volume_mount::Entity::insert(am).exec(&state.db.conn).await {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": format!("Database error: {e}") }),
        )
        .await;
    }
    let value = serde_json::json!({
        "name": req.name,
        "host_path": req.host_path,
        "container_path": req.container_path,
        "read_only": req.read_only,
        "scope": req.scope,
        "target": req.target,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await
}

/// `PUT /api/admin/volume_mounts/{name}` — update a volume mount.
async fn update_volume_mount(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(name): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.mounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: UpdateVolumeMountRequest = match serde_json::from_value(decrypted) {
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
    let existing = entities::volume_mount::Entity::find_by_id(name.clone())
        .one(&state.db.conn)
        .await
        .ok()
        .flatten();
    let Some(existing) = existing else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("Volume mount '{name}' not found.") }),
        )
        .await;
    };
    if !VALID_SCOPES.contains(&req.scope.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "scope must be 'none', 'user', or 'group'." }),
        )
        .await;
    }
    if req.scope == "user" && user_manager::get_user(&state.db, &req.target).await.unwrap_or(None).is_none() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("User '{}' not found.", req.target) }),
        )
        .await;
    }
    if req.scope == "group" && user_manager::get_group(&state.db, &req.target).await.unwrap_or(None).is_none() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("Group '{}' not found.", req.target) }),
        )
        .await;
    }
    let am = entities::volume_mount::ActiveModel {
        name: ActiveValue::Set(existing.name),
        host_path: ActiveValue::Set(req.host_path.clone()),
        container_path: ActiveValue::Set(req.container_path.clone()),
        read_only: ActiveValue::Set(req.read_only),
        scope: ActiveValue::Set(req.scope.clone()),
        target: ActiveValue::Set(req.target.clone()),
    };
    if let Err(e) = entities::volume_mount::Entity::update(am).exec(&state.db.conn).await {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": format!("Database error: {e}") }),
        )
        .await;
    }
    let value = serde_json::json!({
        "name": name,
        "host_path": req.host_path,
        "container_path": req.container_path,
        "read_only": req.read_only,
        "scope": req.scope,
        "target": req.target,
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `DELETE /api/admin/volume_mounts/{name}` — delete a volume mount.
async fn delete_volume_mount(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(name): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.mounts") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let existing = entities::volume_mount::Entity::find_by_id(name.clone())
        .one(&state.db.conn)
        .await
        .ok()
        .flatten();
    let Some(existing) = existing else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("Volume mount '{name}' not found.") }),
        )
        .await;
    };
    let am = entities::volume_mount::ActiveModel {
        name: ActiveValue::Set(existing.name),
        host_path: ActiveValue::NotSet,
        container_path: ActiveValue::NotSet,
        read_only: ActiveValue::NotSet,
        scope: ActiveValue::NotSet,
        target: ActiveValue::NotSet,
    };
    if let Err(e) = entities::volume_mount::Entity::delete(am).exec(&state.db.conn).await {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": format!("Database error: {e}") }),
        )
        .await;
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await
}

// ---------------------------------------------------------------------------
// Roles + permissions
// ---------------------------------------------------------------------------

/// `GET /api/admin/permissions` — the full permission catalog.
async fn list_permissions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.roles") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let _ = &state;
    let admin: Vec<Value> = permissions::ADMIN_PERMISSIONS
        .iter()
        .map(|p| serde_json::json!({ "name": p, "description": permission_description(p) }))
        .collect();
    let user_perms: Vec<Value> = permissions::USER_PERMISSIONS
        .iter()
        .map(|p| serde_json::json!({ "name": p, "description": permission_description(p) }))
        .collect();
    let value = serde_json::json!({ "admin": admin, "user": user_perms });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `GET /api/admin/roles` — list all roles.
async fn list_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.roles") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let roles = user_manager::get_all_roles(&state.db).await.unwrap_or_default();
    let value = serde_json::to_value(&roles).unwrap_or_default();
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/admin/roles` — create a role.
async fn create_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.roles") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: CreateRoleRequest = match serde_json::from_value(decrypted) {
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
    match user_manager::create_role(&state.db, &req.name, &req.description, &req.permissions).await {
        Ok(role) => {
            let value = serde_json::to_value(&role).unwrap_or_default();
            e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await
        }
        // The Python endpoint maps every ValueError (duplicate name, invalid
        // name, unknown permission) to a 409.
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::CONFLICT,
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

/// `PUT /api/admin/roles/{role_name}` — update a role.
async fn update_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(role_name): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.roles") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
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
    let req: UpdateRoleRequest = match serde_json::from_value(decrypted) {
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
    match user_manager::update_role(
        &state.db,
        &role_name,
        req.description.as_deref(),
        req.permissions.as_deref(),
    )
    .await
    {
        Ok(role) => {
            let value = serde_json::to_value(&role).unwrap_or_default();
            e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
        }
        // The Python endpoint maps every ValueError (unknown role, unknown
        // permission) to a 404.
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

/// `DELETE /api/admin/roles/{role_name}` — delete a role.
async fn delete_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(role_name): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.roles") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    match user_manager::delete_role(&state.db, &role_name).await {
        Ok(()) => e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await,
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("still assigned") || msg.contains("built-in") {
                StatusCode::CONFLICT
            } else {
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::NOT_FOUND)
            };
            e2ee::encrypt_response(&state, &headers, status, serde_json::json!({ "detail": msg })).await
        }
    }
}

// ---------------------------------------------------------------------------
// Backup / restore
// ---------------------------------------------------------------------------

/// `POST /api/admin/backup/create` — create a backup archive.
async fn create_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.backup") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    };
    let req: CreateBackupRequest = match serde_json::from_value(decrypted) {
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
    match crate::backup_manager::create_backup(&state, req.include_user_data).await {
        Ok(value) => e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, value).await,
        Err(e) => e2ee::encrypt_response(&state, &headers, StatusCode::BAD_REQUEST, serde_json::json!({ "detail": e.to_string() })).await,
    }
}

/// `GET /api/admin/backup` — list backup archives.
async fn list_backups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.backup") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let value = crate::backup_manager::list_backups(&state.settings);
    // Python returns the bare list (no wrapper object).
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::Value::Array(value)).await
}

/// `GET /api/admin/backup/{name}/download` — download one chunk of a backup.
async fn download_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(name): PathParam<String>,
    Query(q): Query<BackupDownloadQuery>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.backup") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    match crate::backup_manager::get_backup_chunk(&state.settings, &name, q.chunk_index) {
        Ok((data, is_last)) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::OK,
                serde_json::json!({
                    "chunk_data_b64": b64,
                    "is_last_chunk": is_last,
                    "filename": name,
                }),
            )
            .await
        }
        Err(e) => e2ee::encrypt_response(&state, &headers, StatusCode::NOT_FOUND, serde_json::json!({ "detail": e.to_string() })).await,
    }
}

/// `POST /api/admin/backup/restore` — restore from a stored backup.
async fn restore_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.backup") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    };
    let Some(name) = decrypted.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "Missing 'name' to restore." }),
        )
        .await
    };
    match crate::backup_manager::restore_from_stored(&state, name).await {
        Ok(value) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await,
        Err(e) => e2ee::encrypt_response(&state, &headers, StatusCode::BAD_REQUEST, serde_json::json!({ "detail": e.to_string() })).await,
    }
}

/// `POST /api/admin/backup/restore/initiate` — start a chunked restore upload.
async fn backup_restore_initiate(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.backup") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let upload_id = crate::catalog::random_uuid();
    let upload_path = crate::backup_manager::get_backup_dir(&state.settings).join(format!("restore_{upload_id}"));
    std::fs::create_dir_all(&upload_path).ok();
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::json!({ "upload_id": upload_id })).await
}

/// `POST /api/admin/backup/restore/chunk` — upload one chunk of a restore.
async fn backup_restore_chunk(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.backup") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    };
    let req: BackupRestoreChunkRequest = match serde_json::from_value(decrypted) {
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
    let upload_path = crate::backup_manager::get_backup_dir(&state.settings).join(format!("restore_{}", req.upload_id));
    if !upload_path.is_dir() {
        return e2ee::encrypt_response(&state, &headers, StatusCode::NOT_FOUND, serde_json::json!({ "detail": "Restore session not found." })).await;
    }
    let data = match base64::engine::general_purpose::STANDARD.decode(&req.chunk_data_b64) {
        Ok(d) => d,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "detail": format!("Invalid Base64 chunk data: {e}") }),
            )
            .await
        }
    };
    let chunk_path = upload_path.join(format!("chunk_{}", req.chunk_index));
    if let Err(e) = std::fs::write(&chunk_path, data) {
        return e2ee::encrypt_response(&state, &headers, StatusCode::BAD_REQUEST, serde_json::json!({ "detail": format!("Could not write chunk: {e}") })).await;
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::json!({ "status": "ok", "chunk_index": req.chunk_index })).await
}

/// `POST /api/admin/backup/restore/finalize` — finish a chunked restore.
async fn backup_restore_finalize(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.backup") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    };
    let req: BackupRestoreFinalizeRequest = match serde_json::from_value(decrypted) {
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
    let upload_path = crate::backup_manager::get_backup_dir(&state.settings).join(format!("restore_{}", req.upload_id));
    if !upload_path.is_dir() {
        return e2ee::encrypt_response(&state, &headers, StatusCode::NOT_FOUND, serde_json::json!({ "detail": "Restore session not found." })).await;
    }
    let mut assembled: Vec<u8> = Vec::new();
    let mut assemble_err: Option<(StatusCode, String)> = None;
    for i in 0..req.total_chunks {
        let chunk_path = upload_path.join(format!("chunk_{i}"));
        if !chunk_path.is_file() {
            assemble_err = Some((StatusCode::BAD_REQUEST, format!("Missing chunk {i}.")));
            break;
        }
        match std::fs::read(&chunk_path) {
            Ok(data) => assembled.extend_from_slice(&data),
            Err(e) => {
                assemble_err = Some((StatusCode::BAD_REQUEST, format!("Could not read chunk {i}: {e}")));
                break;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&upload_path);
    if let Some((s, d)) = assemble_err {
        return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await;
    }
    match crate::backup_manager::restore_from_bytes(&state, &assembled).await {
        Ok(value) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await,
        Err(e) => e2ee::encrypt_response(&state, &headers, StatusCode::BAD_REQUEST, serde_json::json!({ "detail": e.to_string() })).await,
    }
}

/// `DELETE /api/admin/backup/{name}` — delete a backup archive.
async fn delete_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(name): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !require_permission(&user, "admin.backup") {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Admin privileges required." }),
        )
        .await;
    }
    match crate::backup_manager::delete_backup(&state.settings, &name) {
        Ok(()) => e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await,
        Err(e) => e2ee::encrypt_response(&state, &headers, StatusCode::NOT_FOUND, serde_json::json!({ "detail": e.to_string() })).await,
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// The admin router. Mounted under `/api/admin` (see `app.rs`).
pub fn admin_router() -> Router<AppState> {
    Router::new()
        // Roster data + global settings
        .route("/data", post(get_management_data))
        .route("/global-settings", get(get_global_settings).post(save_global_settings))
        // SSO / OIDC
        .route("/sso-groups", get(get_sso_groups))
        .route("/sso", get(get_sso_config).put(save_sso_config))
        .route("/sso/test", post(test_sso_connection))
        // Certificates (Let's Encrypt)
        .route("/certificates", get(get_certificates).put(save_certificates))
        .route("/certificates/renew", post(renew_certificates))
        // App stores
        .route("/apps/stores", get(get_app_stores).post(add_app_store))
        .route("/apps/stores/{store_name}", delete(delete_app_store))
        // Meta apps
        .route("/apps/meta", post(create_meta_app))
        .route("/launch/meta_customize", post(launch_meta_for_customization))
        // Available apps
        .route("/apps/available", get(get_available_apps))
        // Installed apps
        .route("/apps/installed", get(list_installed_apps).post(install_app))
        .route("/apps/installed/{app_id}", put(update_installed_app).delete(delete_installed_app))
        .route("/apps/installed/{app_id}/check_update", post(check_app_update))
        .route("/apps/installed/check_all_updates", post(check_all_app_updates))
        .route("/apps/installed/pull_all_latest", post(pull_all_app_images))
        .route("/apps/installed/{app_id}/pull_latest", post(pull_latest_app_image))
        .route("/apps/auto_update_status", get(get_auto_update_status))
        // Templates
        .route("/apps/templates", get(get_app_templates).post(save_app_template))
        .route("/apps/templates/schema", get(get_template_schema).post(save_template_schema))
        .route("/apps/templates/{template_name}", delete(delete_app_template))
        // Sessions
        .route("/sessions", get(get_all_sessions))
        .route("/sessions/{session_id}", delete(stop_any_session))
        .route("/sessions/{session_id}/recreate", post(recreate_any_session))
        // OIDC revoke
        .route("/oidc/revoke", post(revoke_oidc_sessions))
        // People
        .route("/people", post(create_person))
        .route("/people/{username}", put(update_person).delete(delete_person))
        .route("/people/{username}/admin-status", post(set_admin_status))
        .route("/people/{username}/homedirs", get(list_person_home_dirs).post(create_person_home_dir))
        .route("/people/{username}/homedirs/{home_name}", delete(delete_person_home_dir))
        .route("/people/{username}/access", post(set_user_access))
        // Groups
        .route("/groups", post(create_group))
        .route("/groups/{group_name}", put(update_group).delete(delete_group))
        // Volume mounts
        .route("/volume_mounts", get(list_volume_mounts).post(create_volume_mount))
        .route("/volume_mounts/{name}", put(update_volume_mount).delete(delete_volume_mount))
        // Roles + permissions
        .route("/permissions", get(list_permissions))
        .route("/roles", get(list_roles).post(create_role))
        .route("/roles/{role_name}", put(update_role).delete(delete_role))
        // Backup / restore
        .route("/backup/create", post(create_backup))
        .route("/backup", get(list_backups))
        .route("/backup/restore", post(restore_backup))
        .route("/backup/restore/initiate", post(backup_restore_initiate))
        .route("/backup/restore/chunk", post(backup_restore_chunk))
        .route("/backup/restore/finalize", post(backup_restore_finalize))
        .route("/backup/{name}/download", get(download_backup))
        .route("/backup/{name}", delete(delete_backup))
}
