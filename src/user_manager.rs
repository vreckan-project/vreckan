//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! User / group / role management (RBAC).
//!
//! Ports `server/user_manager`. This is the runtime source of truth for
//! accounts, groups, and roles. Unlike the Python code — which keeps in-memory
//! dicts (`USER_DATA` / `GROUP_DATA` / `ROLE_DATA`) as the source of truth and
//! mirrors them to the database — this port queries the configuration database
//! directly on every read and writes back on every mutation. The database is
//! the single source of truth.
//!
//! The module is built up in groups:
//!   1. constants + DB persistence helpers (this section),
//!   2. bootstrap + migrations + loaders,
//!   3. getters + permission resolution,
//!   4. user / group CRUD,
//!   5. home dirs + passwords + SSO,
//!   6. roles.

use std::collections::HashSet;
use std::sync::OnceLock;

use argon2::{
    password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier},
    Argon2,
};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr,
    EntityTrait, IntoActiveModel, QueryFilter, QueryTrait,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use hex;

use crate::db::Db;
use crate::entities;
use crate::permissions;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Error type
//
// The Python code raises `ValueError` for both validation failures and
// not-found cases; the API layer maps those to HTTP 400/404. Here we keep the
// two distinct so the API layer can map them to the correct status code.
// ---------------------------------------------------------------------------

/// Errors surfaced by the user/group/role management functions.
#[derive(Debug)]
pub enum UserError {
    /// A validation failure (bad input, duplicate name, unknown role, ...).
    /// Maps to HTTP 400.
    Validation(String),
    /// The caller is not authenticated (missing/invalid token). Maps to 401.
    Unauthorized(String),
    /// The referenced user / group / role does not exist. Maps to HTTP 404.
    NotFound(String),
    /// A database error. Maps to HTTP 500.
    Db(DbErr),
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserError::Validation(m) => write!(f, "{m}"),
            UserError::Unauthorized(m) => write!(f, "{m}"),
            UserError::NotFound(m) => write!(f, "{m}"),
            UserError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for UserError {}

impl From<DbErr> for UserError {
    fn from(e: DbErr) -> Self {
        UserError::Db(e)
    }
}

impl UserError {
    /// The HTTP status code this error maps to (used by the API layer).
    pub fn status(&self) -> u16 {
        match self {
            UserError::Validation(_) => 400,
            UserError::Unauthorized(_) => 401,
            UserError::NotFound(_) => 404,
            UserError::Db(_) => 500,
        }
    }
}

// ---------------------------------------------------------------------------
// Record types
//
// Clean, serialisable shapes returned by the getters. The SeaORM entity models
// are converted into these once, at the boundary, so the rest of the codebase
// (permission resolution, the API layer) never touches ORM types and always
// works with plain `Vec<String>` lists.
// ---------------------------------------------------------------------------

/// A user or admin account.
#[derive(Clone, Debug, Serialize)]
pub struct UserRecord {
    pub username: String,
    pub is_admin: bool,
    pub is_sso: bool,
    /// Per-user settings (None for accounts with no explicit settings).
    pub settings: Option<serde_json::Value>,
    /// argon2id hash of the web-login password (None if unset).
    pub password_hash: Option<String>,
    /// True while the account still carries its initial (bootstrap)
    /// password; cleared by any explicit password change.
    pub must_change_password: bool,
    /// Provider (IdP) groups the SSO user belongs to.
    pub sso_groups: Vec<String>,
    /// Roles assigned directly to the user.
    pub roles: Vec<String>,
    /// Permissions granted directly to the user.
    pub permissions: Vec<String>,
    /// Groups the user belongs to (first entry is the primary group).
    pub groups: Vec<String>,
}

/// A group definition (settings + role/permission bundles for its members).
#[derive(Clone, Debug, Serialize)]
pub struct GroupRecord {
    pub name: String,
    pub settings: serde_json::Value,
    pub roles: Vec<String>,
    pub permissions: Vec<String>,
}

/// A named bundle of permissions that can be assigned to users and groups.
#[derive(Clone, Debug, Serialize)]
pub struct RoleRecord {
    pub name: String,
    pub description: String,
    pub permissions: Vec<String>,
    pub is_builtin: bool,
}

/// Convert a JSON column value into a `Vec<String>`.
///
/// The `roles` / `permissions` / `groups` / `sso_groups` columns are stored as
/// JSON arrays of strings. On Postgres they come back as native JSON; on
/// SQLite/MySQL they come back as a TEXT string that must be parsed. A `None`
/// (NULL) value becomes an empty list, matching the Python `... or []`
/// normalisation.
fn json_str_list(v: Option<serde_json::Value>) -> Vec<String> {
    match v {
        None => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|i| i.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::String(s)) => {
            serde_json::from_str::<Vec<String>>(&s).unwrap_or_default()
        }
        Some(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Module-level state
//
// The Python module keeps a few process-wide values (the server's public key
// for log messages, and the externally-visible ports). These are set once at
// startup and read by the auth / session code.
// ---------------------------------------------------------------------------

/// Process-wide values set at startup (mirrors the Python module globals).
struct ServerState {
    /// The server's public key PEM, for log messages.
    pub server_public_key_pem: Option<String>,
    /// The externally-visible API port.
    pub external_api_port: i64,
    /// The externally-visible session-proxy port.
    pub external_session_port: i64,
}

static SERVER_STATE: OnceLock<ServerState> = OnceLock::new();

/// Set the server's public key for use in log messages.
pub fn set_server_public_key(key: &str) {
    let mut state = ServerState {
        server_public_key_pem: Some(key.to_string()),
        external_api_port: 8000,
        external_session_port: 8443,
    };
    if let Some(existing) = SERVER_STATE.get() {
        state.external_api_port = existing.external_api_port;
        state.external_session_port = existing.external_session_port;
    }
    let _ = SERVER_STATE.set(state);
}

/// Set the externally-visible API and session-proxy ports.
pub fn set_external_ports(api_port: i64, session_port: i64) {
    let mut state = ServerState {
        server_public_key_pem: None,
        external_api_port: api_port,
        external_session_port: session_port,
    };
    if let Some(existing) = SERVER_STATE.get() {
        state.server_public_key_pem = existing.server_public_key_pem.clone();
    }
    let _ = SERVER_STATE.set(state);
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The built-in roles seeded on first run. They are fully editable (an admin
/// can change their permission sets) but flagged `is_builtin` so the UI can
/// show they ship with the application.
pub const BUILTIN_ROLES: [(&str, &str, &[&str]); 3] = [
    (
        "admin",
        "Full administrator. Grants every admin permission.",
        &["admin"],
    ),
    (
        "operator",
        "Manages apps, stores, and sessions; can use GPU and storage.",
        &["admin.apps", "admin.stores", "admin.sessions", "user.gpu", "user.storage"],
    ),
    (
        "user",
        "Standard user. Can use persistent storage and the GPU.",
        &["user.storage", "user.gpu"],
    ),
];

/// The default per-user settings. Every user's effective settings are computed
/// by starting from this dict and overlaying the user's (and their primary
/// group's) explicit values.
static DEFAULT_USER_SETTINGS: OnceLock<serde_json::Value> = OnceLock::new();

/// A reference to the default per-user settings (lazily built once).
pub fn default_user_settings() -> &'static serde_json::Value {
    DEFAULT_USER_SETTINGS.get_or_init(|| {
        serde_json::json!({
            "active": true,
            "group": "none",
            "persistent_storage": true,
            "public_sharing": false,
            "harden_container": false,
            "harden_openbox": false,
            "gpu": true,
            "storage_limit": -1,
            "session_limit": -1,
            // Personal UI language preference (null = follow the browser). Not
            // an admin-managed capability; set via the sidebar language selector.
            "ui_language": null,
        })
    })
}

/// The capability settings that are driven by permissions. Each maps a
/// settings-form field (a boolean capability) to the permission that grants
/// it. A user/group "has" the capability iff the corresponding permission is
/// in their effective (union) permission set.
pub const CAPABILITY_PERMISSIONS: [(&str, &str); 5] = [
    ("persistent_storage", "user.storage"),
    ("public_sharing", "user.sharing"),
    ("harden_container", "user.harden_container"),
    ("harden_openbox", "user.harden_openbox"),
    ("gpu", "user.gpu"),
];

// ---------------------------------------------------------------------------
// Database persistence helpers
//
// These keep the configuration database in sync with the logical operations
// above. All are async because the database layer is async.
// ---------------------------------------------------------------------------

/// The optional fields that can be set on a user row by a partial update.
///
/// Mirrors the Python `_db_save_user_row(username, **fields)`: only the
/// provided (non-None) fields are written; the rest are left untouched.
#[derive(Default)]
pub struct UserFields {
    pub is_admin: Option<bool>,
    pub is_sso: Option<bool>,
    pub settings: Option<serde_json::Value>,
    pub password_hash: Option<String>,
    pub must_change_password: Option<bool>,
    pub sso_groups: Option<Vec<String>>,
    pub roles: Option<Vec<String>>,
    pub permissions: Option<Vec<String>>,
    pub groups: Option<Vec<String>>,
}

/// Insert or update a user row, setting only the provided (non-None) fields.
///
/// The existing row is loaded first (so untouched fields keep their current
/// values), the provided fields are applied, and the result is upserted. This
/// mirrors the Python helper, which loads the row, sets the provided fields,
/// and commits.
pub async fn db_save_user_row(
    conn: &DatabaseConnection,
    username: &str,
    fields: &UserFields,
) -> Result<(), DbErr> {
    let existing = entities::user::Entity::find_by_id(username.to_string())
        .one(conn)
        .await?;
    let mut am = match existing {
        Some(m) => m.into_active_model(),
        None => entities::user::ActiveModel {
            username: ActiveValue::Set(username.to_string()),
            is_admin: ActiveValue::NotSet,
            is_sso: ActiveValue::NotSet,
            settings: ActiveValue::NotSet,
            password_hash: ActiveValue::NotSet,
            must_change_password: ActiveValue::NotSet,
            sso_groups: ActiveValue::NotSet,
            roles: ActiveValue::NotSet,
            permissions: ActiveValue::NotSet,
            groups: ActiveValue::NotSet,
        },
    };
    if let Some(v) = fields.is_admin {
        am.is_admin = ActiveValue::Set(v);
    }
    if let Some(v) = fields.is_sso {
        am.is_sso = ActiveValue::Set(v);
    }
    if let Some(v) = &fields.settings {
        am.settings = ActiveValue::Set(Some(v.clone()));
    }
    if let Some(v) = &fields.password_hash {
        am.password_hash = ActiveValue::Set(Some(v.clone()));
    }
    if let Some(v) = fields.must_change_password {
        am.must_change_password = ActiveValue::Set(v);
    }
    if let Some(v) = &fields.sso_groups {
        am.sso_groups = ActiveValue::Set(Some(serde_json::to_value(v).unwrap_or_default()));
    }
    if let Some(v) = &fields.roles {
        am.roles = ActiveValue::Set(Some(serde_json::to_value(v).unwrap_or_default()));
    }
    if let Some(v) = &fields.permissions {
        am.permissions = ActiveValue::Set(Some(serde_json::to_value(v).unwrap_or_default()));
    }
    if let Some(v) = &fields.groups {
        am.groups = ActiveValue::Set(Some(serde_json::to_value(v).unwrap_or_default()));
    }

    // Upsert: insert, or update the existing row if the username is present.
    // The loaded row has every field Set, so updating all non-PK columns is
    // safe (untouched fields keep their current values).
    let backend = conn.get_database_backend();
    let stmt = if fields.is_admin.is_some()
        || fields.is_sso.is_some()
        || fields.settings.is_some()
        || fields.password_hash.is_some()
        || fields.must_change_password.is_some()
        || fields.sso_groups.is_some()
        || fields.roles.is_some()
        || fields.permissions.is_some()
        || fields.groups.is_some()
    {
        entities::user::Entity::insert(am)
            .on_conflict(
                OnConflict::column(entities::user::Column::Username)
                    .update_columns([
                        entities::user::Column::IsAdmin,
                        entities::user::Column::IsSso,
                        entities::user::Column::Settings,
                        entities::user::Column::PasswordHash,
                        entities::user::Column::MustChangePassword,
                        entities::user::Column::SsoGroups,
                        entities::user::Column::Roles,
                        entities::user::Column::Permissions,
                        entities::user::Column::Groups,
                    ])
                    .to_owned(),
            )
            .build(backend)
    } else {
        // No fields provided: insert if absent, otherwise leave the row alone.
        entities::user::Entity::insert(am)
            .on_conflict(
                OnConflict::column(entities::user::Column::Username)
                    .do_nothing()
                    .to_owned(),
            )
            .build(backend)
    };
    conn.execute_raw(stmt).await?;
    Ok(())
}

/// Delete a user row (no-op if it does not exist).
async fn db_delete_user_row(conn: &DatabaseConnection, username: &str) -> Result<(), DbErr> {
    let am = entities::user::ActiveModel {
        username: ActiveValue::Set(username.to_string()),
        is_admin: ActiveValue::NotSet,
        is_sso: ActiveValue::NotSet,
        settings: ActiveValue::NotSet,
        password_hash: ActiveValue::NotSet,
        must_change_password: ActiveValue::NotSet,
        sso_groups: ActiveValue::NotSet,
        roles: ActiveValue::NotSet,
        permissions: ActiveValue::NotSet,
        groups: ActiveValue::NotSet,
    };
    entities::user::Entity::delete(am).exec(conn).await?;
    Ok(())
}

/// Insert or update a group row (settings, and optionally roles/permissions).
pub async fn db_save_group(
    conn: &DatabaseConnection,
    group_name: &str,
    group_settings: &serde_json::Value,
    roles: Option<&[String]>,
    permissions: Option<&[String]>,
) -> Result<(), DbErr> {
    let existing = entities::group::Entity::find_by_id(group_name.to_string())
        .one(conn)
        .await?;
    let mut am = match existing {
        Some(m) => m.into_active_model(),
        None => entities::group::ActiveModel {
            name: ActiveValue::Set(group_name.to_string()),
            settings: ActiveValue::NotSet,
            roles: ActiveValue::NotSet,
            permissions: ActiveValue::NotSet,
        },
    };
    am.settings = ActiveValue::Set(group_settings.clone());
    if let Some(r) = roles {
        am.roles = ActiveValue::Set(Some(serde_json::to_value(r).unwrap_or_default()));
    }
    if let Some(p) = permissions {
        am.permissions = ActiveValue::Set(Some(serde_json::to_value(p).unwrap_or_default()));
    }

    let backend = conn.get_database_backend();
    let stmt = entities::group::Entity::insert(am)
        .on_conflict(
            OnConflict::column(entities::group::Column::Name)
                .update_columns([
                    entities::group::Column::Settings,
                    entities::group::Column::Roles,
                    entities::group::Column::Permissions,
                ])
                .to_owned(),
        )
        .build(backend);
    conn.execute_raw(stmt).await?;
    Ok(())
}

/// Delete a group row (no-op if it does not exist).
async fn db_delete_group(conn: &DatabaseConnection, group_name: &str) -> Result<(), DbErr> {
    let am = entities::group::ActiveModel {
        name: ActiveValue::Set(group_name.to_string()),
        settings: ActiveValue::NotSet,
        roles: ActiveValue::NotSet,
        permissions: ActiveValue::NotSet,
    };
    entities::group::Entity::delete(am).exec(conn).await?;
    Ok(())
}

/// Set (or clear, when `password_hash` is None) a user's password hash.
async fn db_set_password(
    conn: &DatabaseConnection,
    username: &str,
    password_hash: Option<&str>,
) -> Result<(), DbErr> {
    let existing = entities::user::Entity::find_by_id(username.to_string())
        .one(conn)
        .await?;
    let mut am = match existing {
        Some(m) => m.into_active_model(),
        None => entities::user::ActiveModel {
            username: ActiveValue::Set(username.to_string()),
            is_admin: ActiveValue::NotSet,
            is_sso: ActiveValue::NotSet,
            settings: ActiveValue::NotSet,
            password_hash: ActiveValue::NotSet,
            must_change_password: ActiveValue::NotSet,
            sso_groups: ActiveValue::NotSet,
            roles: ActiveValue::NotSet,
            permissions: ActiveValue::NotSet,
            groups: ActiveValue::NotSet,
        },
    };
    am.password_hash = ActiveValue::Set(password_hash.map(String::from));
    // A freshly set password is no longer the bootstrap default: clear the
    // must-change flag (a cleared password leaves the flag untouched).
    if password_hash.is_some() {
        am.must_change_password = ActiveValue::Set(false);
    }

    let backend = conn.get_database_backend();
    let stmt = entities::user::Entity::insert(am)
        .on_conflict(
            OnConflict::column(entities::user::Column::Username)
                .update_columns([
                    entities::user::Column::IsAdmin,
                    entities::user::Column::IsSso,
                    entities::user::Column::Settings,
                    entities::user::Column::PasswordHash,
                    entities::user::Column::MustChangePassword,
                    entities::user::Column::SsoGroups,
                    entities::user::Column::Roles,
                    entities::user::Column::Permissions,
                    entities::user::Column::Groups,
                ])
                .to_owned(),
        )
        .build(backend);
    conn.execute_raw(stmt).await?;
    Ok(())
}

/// Insert or update a role row from a role definition.
async fn db_save_role(
    conn: &DatabaseConnection,
    name: &str,
    description: &str,
    permissions: &[String],
    is_builtin: bool,
) -> Result<(), DbErr> {
    let existing = entities::role::Entity::find_by_id(name.to_string())
        .one(conn)
        .await?;
    let mut am = match existing {
        Some(m) => m.into_active_model(),
        None => entities::role::ActiveModel {
            name: ActiveValue::Set(name.to_string()),
            description: ActiveValue::NotSet,
            permissions: ActiveValue::NotSet,
            is_builtin: ActiveValue::NotSet,
        },
    };
    am.description = ActiveValue::Set(description.to_string());
    am.permissions = ActiveValue::Set(serde_json::to_value(permissions).unwrap_or_default());
    am.is_builtin = ActiveValue::Set(is_builtin);

    let backend = conn.get_database_backend();
    let stmt = entities::role::Entity::insert(am)
        .on_conflict(
            OnConflict::column(entities::role::Column::Name)
                .update_columns([
                    entities::role::Column::Description,
                    entities::role::Column::Permissions,
                    entities::role::Column::IsBuiltin,
                ])
                .to_owned(),
        )
        .build(backend);
    conn.execute_raw(stmt).await?;
    Ok(())
}

/// Delete a role row (no-op if it does not exist).
async fn db_delete_role(conn: &DatabaseConnection, name: &str) -> Result<(), DbErr> {
    let am = entities::role::ActiveModel {
        name: ActiveValue::Set(name.to_string()),
        description: ActiveValue::NotSet,
        permissions: ActiveValue::NotSet,
        is_builtin: ActiveValue::NotSet,
    };
    entities::role::Entity::delete(am).exec(conn).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Bootstrap + migrations + loaders
// ---------------------------------------------------------------------------

/// The permissions of a role (queried from the database).
async fn role_permissions(
    conn: &DatabaseConnection,
    role_name: &str,
) -> Result<Vec<String>, DbErr> {
    let role = entities::role::Entity::find_by_id(role_name.to_string())
        .one(conn)
        .await?;
    Ok(role.map(|r| json_str_list(Some(r.permissions))).unwrap_or_default())
}

/// The well-known default bootstrap password (the code default of
/// `bootstrap_admin_password`).
pub const DEFAULT_BOOTSTRAP_PASSWORD: &str = "admin1234";

/// Bootstrap a default admin account in the database on first run.
///
/// The default admin is seeded with a bootstrap password and marked
/// `must_change_password` so the client forces a change on first login. The
/// log line names the username but deliberately avoids printing the password
/// value (only the well-known default is named, since it is public knowledge).
pub async fn generate_default_admin(db: &Db, settings: &Settings) -> Result<(), DbErr> {
    let conn = &db.conn;
    // If any admin already exists, do nothing.
    let admins = entities::user::Entity::find()
        .filter(entities::user::Column::IsAdmin.eq(true))
        .all(conn)
        .await?;
    if !admins.is_empty() {
        return Ok(());
    }

    let admin_username = if settings.bootstrap_admin_username.is_empty() {
        "admin".to_string()
    } else {
        settings.bootstrap_admin_username.clone()
    };
    let bootstrap_password = if settings.bootstrap_admin_password.is_empty() {
        DEFAULT_BOOTSTRAP_PASSWORD.to_string()
    } else {
        settings.bootstrap_admin_password.clone()
    };
    let password_hash = Argon2::default()
        .hash_password(bootstrap_password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| DbErr::Custom(e.to_string()))?;

    let am = entities::user::ActiveModel {
        username: ActiveValue::Set(admin_username.clone()),
        is_admin: ActiveValue::Set(true),
        is_sso: ActiveValue::NotSet,
        settings: ActiveValue::Set(None),
        password_hash: ActiveValue::Set(Some(password_hash)),
        // The account starts with its initial password: force a change on
        // first login.
        must_change_password: ActiveValue::Set(true),
        sso_groups: ActiveValue::NotSet,
        roles: ActiveValue::NotSet,
        permissions: ActiveValue::NotSet,
        groups: ActiveValue::NotSet,
    };
    am.insert(conn).await?;

    // Record the actual bootstrap username so the protection guards can
    // identify it deterministically, regardless of database row ordering or
    // later environment changes.
    db.set_app_setting("VRECKAN_BOOTSTRAP_ADMIN_USERNAME", &admin_username)
        .await?;

    if bootstrap_password == DEFAULT_BOOTSTRAP_PASSWORD {
        warn!(
            "No admin users found. Created default '{}' admin with the well-known default bootstrap password '{}' — change it after first login.",
            admin_username, DEFAULT_BOOTSTRAP_PASSWORD
        );
    } else {
        warn!(
            "No admin users found. Created default '{}' admin with a configured bootstrap password — change it after first login.",
            admin_username
        );
    }
    Ok(())
}

/// Resolve the bootstrap admin's username.
///
/// Precedence: a value stored in the `app_settings` table (recorded when the
/// account was created) wins over the environment/default. This is
/// deterministic across database backends — unlike picking "an admin" from the
/// users table, whose row order is database-dependent.
pub async fn bootstrap_username(db: &Db, settings: &Settings) -> Result<String, DbErr> {
    match db.get_app_setting("VRECKAN_BOOTSTRAP_ADMIN_USERNAME").await? {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Ok(if settings.bootstrap_admin_username.is_empty() {
            "admin".to_string()
        } else {
            settings.bootstrap_admin_username.clone()
        }),
    }
}

/// One-time migration: give admin accounts that still have NULL settings an
/// explicit default settings dict (active=True). Idempotent: a no-op once no
/// admin has NULL settings.
pub async fn migrate_admin_settings(db: &Db) -> Result<(), DbErr> {
    let conn = &db.conn;
    let users = entities::user::Entity::find().all(conn).await?;
    let mut changed = 0;
    for m in users {
        if m.is_admin && m.settings.is_none() {
            let mut am = m.into_active_model();
            am.settings = ActiveValue::Set(Some(default_user_settings().clone()));
            am.update(conn).await?;
            changed += 1;
        }
    }
    if changed > 0 {
        info!(
            "Migrated {} account(s) from NULL settings to explicit defaults.",
            changed
        );
    }
    Ok(())
}

/// One-time migration from the old single-flag/group model to RBAC.
///
/// Runs on the first startup after the roles/permissions columns are added. It
/// snapshots each account's *current* capability settings into the new
/// permission model so no one loses (or gains) capabilities on upgrade. It only
/// acts while the new columns are still empty (the first run); once an admin
/// has edited roles/permissions in the UI the columns are populated and the
/// migration is a no-op.
pub async fn migrate_rbac(db: &Db) -> Result<(), DbErr> {
    let conn = &db.conn;
    let users = entities::user::Entity::find().all(conn).await?;
    let groups = entities::group::Entity::find().all(conn).await?;

    // Only migrate accounts that have not been touched by the new model yet.
    let any_user_touched = users.iter().any(|u| {
        !json_str_list(u.roles.clone()).is_empty()
            || !json_str_list(u.permissions.clone()).is_empty()
            || !json_str_list(u.groups.clone()).is_empty()
    });
    if any_user_touched {
        return Ok(());
    }
    let any_group_touched = groups.iter().any(|g| {
        !json_str_list(g.roles.clone()).is_empty()
            || !json_str_list(g.permissions.clone()).is_empty()
    });
    if any_group_touched {
        return Ok(());
    }

    let user_role_perms = role_permissions(conn, "user").await?;

    let mut changed_users = 0;
    for m in users {
        let settings = m.settings.clone().unwrap_or_default();
        let is_admin = m.is_admin;
        let (roles, permissions) = if is_admin {
            (vec!["admin".to_string()], Vec::new())
        } else {
            // The "user" role grants user.storage + user.gpu. Add direct
            // permissions for any capability the settings grant that the role
            // does not, and (for capabilities the role grants) a direct
            // permission only when the setting is explicitly enabled.
            let mut perms = Vec::new();
            for (field, perm) in CAPABILITY_PERMISSIONS {
                let value = settings.get(field);
                if value.is_none() {
                    continue; // not explicitly set -> role/default decides
                }
                if value.and_then(|v| v.as_bool()).unwrap_or(false)
                    && !user_role_perms.iter().any(|p| p == perm)
                {
                    perms.push(perm.to_string());
                }
            }
            (vec!["user".to_string()], perms)
        };
        // Move the single group into the multi-group list.
        let groups_list = {
            let primary = settings.get("group").and_then(|v| v.as_str()).unwrap_or("");
            if primary.is_empty() || primary == "none" {
                Vec::new()
            } else {
                vec![primary.to_string()]
            }
        };

        let mut am = m.into_active_model();
        am.roles = ActiveValue::Set(Some(serde_json::to_value(&roles).unwrap_or_default()));
        am.permissions =
            ActiveValue::Set(Some(serde_json::to_value(&permissions).unwrap_or_default()));
        am.groups = ActiveValue::Set(Some(serde_json::to_value(&groups_list).unwrap_or_default()));
        am.update(conn).await?;
        changed_users += 1;
    }

    // Snapshot each group's capability settings into its permissions so
    // group-level capability overrides keep applying to members.
    let mut changed_groups = 0;
    for g in groups {
        let gsettings = g.settings.clone();
        let mut perms = Vec::new();
        for (field, perm) in CAPABILITY_PERMISSIONS {
            if gsettings.get(field).and_then(|v| v.as_bool()).unwrap_or(false) {
                perms.push(perm.to_string());
            }
        }
        if !perms.is_empty() {
            let mut gam = g.into_active_model();
            gam.permissions =
                ActiveValue::Set(Some(serde_json::to_value(&perms).unwrap_or_default()));
            gam.update(conn).await?;
            changed_groups += 1;
        }
    }

    if changed_users > 0 || changed_groups > 0 {
        info!(
            "RBAC migration: snapshotted capabilities for {} user(s) and {} group(s).",
            changed_users, changed_groups
        );
    }
    Ok(())
}

/// Load roles from the database, seeding the built-in roles on first run.
pub async fn load_roles(db: &Db) -> Result<(), DbErr> {
    let conn = &db.conn;
    let rows = entities::role::Entity::find().all(conn).await?;
    if rows.is_empty() {
        // First run: seed the built-in roles.
        for (name, description, perms) in BUILTIN_ROLES {
            let am = entities::role::ActiveModel {
                name: ActiveValue::Set(name.to_string()),
                description: ActiveValue::Set(description.to_string()),
                permissions: ActiveValue::Set(serde_json::to_value(perms).unwrap_or_default()),
                is_builtin: ActiveValue::Set(true),
            };
            am.insert(conn).await?;
        }
    }
    let count = entities::role::Entity::find().all(conn).await?.len();
    info!("Loaded {} role(s) from the database.", count);
    Ok(())
}

/// Load users, admins, groups, and roles from the configuration database.
///
/// Bootstraps a default admin on first run, loads the roles, then runs the
/// settings + RBAC migrations.
pub async fn load_users_and_groups(db: &Db, settings: &Settings) -> Result<(), DbErr> {
    info!("Loading users, admins, groups, and roles from the configuration database...");
    generate_default_admin(db, settings).await?;
    load_roles(db).await?;
    migrate_admin_settings(db).await?;
    migrate_rbac(db).await?;
    let users = entities::user::Entity::find().all(&db.conn).await?.len();
    let groups = entities::group::Entity::find().all(&db.conn).await?.len();
    let roles = entities::role::Entity::find().all(&db.conn).await?.len();
    info!(
        "Loaded {} user(s), {} group(s), {} role(s) from the database.",
        users, groups, roles
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Getters + permission resolution
//
// A user's effective permissions are the union (most-permissive-wins) of:
//   * the permissions assigned directly to the user,
//   * the permissions of the user's roles,
//   * the permissions (and roles) of every group the user belongs to.
// The ``admin`` super-permission expands to the whole catalog. ``is_admin`` is
// derived from this set (it is True iff ``admin`` is in it).
// ---------------------------------------------------------------------------

/// Convert a user model into a clean, serialisable record.
fn model_to_user_record(m: entities::user::Model) -> UserRecord {
    UserRecord {
        username: m.username,
        is_admin: m.is_admin,
        is_sso: m.is_sso,
        settings: m.settings,
        password_hash: m.password_hash,
        must_change_password: m.must_change_password,
        sso_groups: json_str_list(m.sso_groups),
        roles: json_str_list(m.roles),
        permissions: json_str_list(m.permissions),
        groups: json_str_list(m.groups),
    }
}

/// Convert a group model into a clean, serialisable record.
fn model_to_group_record(m: entities::group::Model) -> GroupRecord {
    GroupRecord {
        name: m.name,
        settings: m.settings,
        roles: json_str_list(m.roles),
        permissions: json_str_list(m.permissions),
    }
}

/// Convert a role model into a clean, serialisable record.
fn model_to_role_record(m: entities::role::Model) -> RoleRecord {
    RoleRecord {
        name: m.name,
        description: m.description,
        permissions: json_str_list(Some(m.permissions)),
        is_builtin: m.is_builtin,
    }
}

/// Look up a group by name (internal helper; returns a DB error).
pub async fn get_group_row(
    conn: &DatabaseConnection,
    name: &str,
) -> Result<Option<GroupRecord>, DbErr> {
    let m = entities::group::Entity::find_by_id(name.to_string())
        .one(conn)
        .await?;
    Ok(m.map(model_to_group_record))
}

/// Look up a user by username (None if not found).
pub async fn get_user(db: &Db, username: &str) -> Result<Option<UserRecord>, UserError> {
    let m = entities::user::Entity::find_by_id(username.to_string())
        .one(&db.conn)
        .await?;
    Ok(m.map(model_to_user_record))
}

/// Look up a group by name (None if not found).
pub async fn get_group(db: &Db, name: &str) -> Result<Option<GroupRecord>, UserError> {
    Ok(get_group_row(&db.conn, name).await?)
}

/// All non-admin users.
pub async fn get_all_users(db: &Db) -> Result<Vec<UserRecord>, UserError> {
    let users = entities::user::Entity::find().all(&db.conn).await?;
    Ok(users
        .into_iter()
        .filter(|u| !u.is_admin)
        .map(model_to_user_record)
        .collect())
}

/// All admin users.
pub async fn get_all_admins(db: &Db) -> Result<Vec<UserRecord>, UserError> {
    let users = entities::user::Entity::find().all(&db.conn).await?;
    Ok(users
        .into_iter()
        .filter(|u| u.is_admin)
        .map(model_to_user_record)
        .collect())
}

/// All groups.
pub async fn get_all_groups(db: &Db) -> Result<Vec<GroupRecord>, UserError> {
    let groups = entities::group::Entity::find().all(&db.conn).await?;
    Ok(groups.into_iter().map(model_to_group_record).collect())
}

/// All roles.
pub async fn get_all_roles(db: &Db) -> Result<Vec<RoleRecord>, UserError> {
    let roles = entities::role::Entity::find().all(&db.conn).await?;
    Ok(roles.into_iter().map(model_to_role_record).collect())
}

/// Look up a role by name (None if not found).
pub async fn get_role(db: &Db, name: &str) -> Result<Option<RoleRecord>, UserError> {
    let m = entities::role::Entity::find_by_id(name.to_string())
        .one(&db.conn)
        .await?;
    Ok(m.map(model_to_role_record))
}

/// Overlay `overlay`'s keys onto a clone of `base` (overlay wins).
fn merge_settings(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    let mut result = base.clone();
    if let (Some(obj), Some(over)) = (result.as_object_mut(), overlay.as_object()) {
        for (k, v) in over {
            obj.insert(k.clone(), v.clone());
        }
    }
    result
}

/// Return the final, calculated settings for a user.
///
/// Two kinds of settings are resolved differently:
///
/// * **Capabilities** (persistent_storage, public_sharing, harden_container,
///   harden_openbox, gpu) are driven by the *permission* system: each is True
///   iff the corresponding ``user.*`` permission is in the user's effective
///   (union) permission set.
/// * **Quotas and account state** (active, storage_limit, session_limit) are
///   resolved from the settings model: per-user settings are the base (defaults
///   fill missing keys) and the user's *primary* group's settings override them
///   — EXCEPT ``active``, where an explicit per-user value always wins over the
///   group, so an account can be individually disabled (or kept active)
///   regardless of its group.
pub async fn get_effective_settings(
    db: &Db,
    username: &str,
) -> Result<serde_json::Value, UserError> {
    let user = get_user(db, username).await?;
    let user_settings = user.as_ref().and_then(|u| u.settings.clone());
    // Backfill defaults first so consumers always see a complete settings dict,
    // then overlay the user's explicit values.
    let mut merged = default_user_settings().clone();
    if let Some(us) = &user_settings {
        merged = merge_settings(&merged, us);
    }

    // Quota / account-state overrides from the user's primary group.
    let group_name = merged
        .get("group")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let mut effective = merged.clone();
    if !group_name.is_empty() && group_name != "none" {
        if let Some(group) = get_group_row(&db.conn, &group_name).await? {
            effective = merge_settings(&effective, &group.settings);
            // Explicit per-user `active` wins over the group's value.
            if let Some(us) = &user_settings {
                if let Some(active) = us.get("active") {
                    effective["active"] = active.clone();
                }
            }
        }
    }

    // Capabilities come from the permission system (union of user + roles +
    // groups), not from the settings dict, so they inherit both ways.
    let perms = get_effective_permissions(db, username).await?;
    for (field, perm) in CAPABILITY_PERMISSIONS {
        effective[field] = serde_json::Value::Bool(perms.iter().any(|p| p == perm));
    }
    Ok(effective)
}

/// Return the user's effective (expanded) permission set as a sorted list.
///
/// Unions the user's direct permissions, their roles, and the permissions and
/// roles of every group they belong to, then expands (so ``admin`` grants the
/// whole catalog). Unknown role/permission names are ignored.
pub async fn get_effective_permissions(
    db: &Db,
    username: &str,
) -> Result<Vec<String>, UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => return Ok(Vec::new()),
    };
    let mut sources: Vec<Vec<String>> = vec![user.permissions.clone()];
    for role in &user.roles {
        sources.push(role_permissions(&db.conn, role).await?);
    }
    for group_name in &user.groups {
        if let Some(group) = get_group_row(&db.conn, group_name).await? {
            sources.push(group.permissions.clone());
            for role in &group.roles {
                sources.push(role_permissions(&db.conn, role).await?);
            }
        }
    }
    let set = permissions::union_permissions(&sources);
    let mut sorted: Vec<String> = set.into_iter().collect();
    sorted.sort();
    Ok(sorted)
}

/// True if the user's effective permissions include ``permission``.
pub async fn has_permission(
    db: &Db,
    username: &str,
    permission: &str,
) -> Result<bool, UserError> {
    let perms = get_effective_permissions(db, username).await?;
    Ok(perms.iter().any(|p| p == permission))
}

/// Re-derive and persist the user's ``is_admin`` flag from their effective
/// permissions. Returns the new value. The flag is stored (not just computed)
/// so it is available without re-resolving and stays consistent in the database.
pub async fn recompute_is_admin(db: &Db, username: &str) -> Result<bool, UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => return Ok(false),
    };
    let perms = get_effective_permissions(db, username).await?;
    let is_admin = perms.iter().any(|p| p == permissions::SUPER_PERMISSION);
    if user.is_admin != is_admin {
        if let Some(m) = entities::user::Entity::find_by_id(username.to_string())
            .one(&db.conn)
            .await?
        {
            let mut am = m.into_active_model();
            am.is_admin = ActiveValue::Set(is_admin);
            am.update(&db.conn).await?;
        }
    }
    Ok(is_admin)
}

/// Return the sorted, de-duplicated list of provider groups that SSO users are
/// currently in (from the sso_groups recorded on each SSO account).
pub async fn observed_sso_groups(db: &Db) -> Result<Vec<String>, UserError> {
    let users = entities::user::Entity::find().all(&db.conn).await?;
    let mut seen = HashSet::new();
    for u in users {
        if u.is_sso {
            for g in json_str_list(u.sso_groups) {
                if !g.is_empty() {
                    seen.insert(g);
                }
            }
        }
    }
    let mut sorted: Vec<String> = seen.into_iter().collect();
    sorted.sort();
    Ok(sorted)
}

// ---------------------------------------------------------------------------
// User / group CRUD
// ---------------------------------------------------------------------------

/// True if `name` matches `^[a-zA-Z0-9_-]+$` (letters, numbers, underscore,
/// or hyphen; non-empty).
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Public wrapper over [`valid_name`] for home-directory names. The API layer
/// uses this to return a 422 (invalid request body) for a malformed name
/// *before* calling `create_home_dir`, mirroring the Pydantic
/// `HomeDirectoryCreate` pattern validation (which would otherwise be
/// swallowed by the 409 duplicate handler).
pub fn is_valid_home_name(name: &str) -> bool {
    valid_name(name)
}

/// Public wrapper over [`valid_name`] for group names. The API layer uses
/// this to return a 422 (invalid request body) for a malformed group name,
/// mirroring the Pydantic `CreateGroupRequest.name` pattern validation.
pub fn is_valid_group_name(name: &str) -> bool {
    valid_name(name)
}

/// Create a new admin account. Admins are always created active: an explicit
/// active=True is stored so the per-user `active` precedence keeps them active
/// even if their group says otherwise.
pub async fn create_admin(
    db: &Db,
    username: &str,
    settings_dict: Option<&serde_json::Value>,
) -> Result<UserRecord, UserError> {
    if !valid_name(username) {
        return Err(UserError::Validation(
            "Invalid username. Use only letters, numbers, underscore, or hyphen.".into(),
        ));
    }
    if get_user(db, username).await?.is_some() {
        return Err(UserError::Validation(format!(
            "User or admin '{username}' already exists."
        )));
    }
    let mut admin_settings = default_user_settings().clone();
    if let Some(sd) = settings_dict {
        admin_settings = merge_settings(&admin_settings, sd);
    }
    admin_settings["active"] = serde_json::Value::Bool(true);

    let fields = UserFields {
        is_admin: Some(true),
        settings: Some(admin_settings),
        roles: Some(vec!["admin".to_string()]),
        ..Default::default()
    };
    db_save_user_row(&db.conn, username, &fields).await?;
    info!("Created admin '{username}'.");
    get_user(db, username)
        .await?
        .ok_or_else(|| UserError::NotFound(format!("User or admin '{username}' not found.")))
}

/// Delete a user or admin. Only the bootstrap account is protected (deleting
/// it would lock everyone out of the admin UI).
pub async fn delete_person(
    db: &Db,
    settings: &Settings,
    username: &str,
) -> Result<(), UserError> {
    let bootstrap = bootstrap_username(db, settings).await?;
    if username == bootstrap {
        return Err(UserError::Validation(format!(
            "The root '{username}' account cannot be deleted."
        )));
    }
    if get_user(db, username).await?.is_none() {
        return Err(UserError::NotFound(format!(
            "User or admin '{username}' not found."
        )));
    }
    let user_storage_path = std::path::Path::new(&settings.storage_path).join(username);
    if user_storage_path.is_dir() {
        std::fs::remove_dir_all(&user_storage_path).map_err(|e| {
            UserError::Validation(format!("Failed to delete storage for '{username}': {e}"))
        })?;
        info!("Deleted storage for '{username}'.");
    }
    db_delete_user_row(&db.conn, username).await?;
    info!("Deleted user or admin '{username}'.");
    Ok(())
}

/// Create a new regular user. New users are always created active.
pub async fn create_user(
    db: &Db,
    username: &str,
    settings_dict: Option<&serde_json::Value>,
) -> Result<UserRecord, UserError> {
    if !valid_name(username) {
        return Err(UserError::Validation(
            "Invalid username. Use only letters, numbers, underscore, or hyphen.".into(),
        ));
    }
    if get_user(db, username).await?.is_some() {
        return Err(UserError::Validation(format!(
            "User '{username}' already exists."
        )));
    }
    let mut user_settings = default_user_settings().clone();
    if let Some(sd) = settings_dict {
        user_settings = merge_settings(&user_settings, sd);
    }
    user_settings["active"] = serde_json::Value::Bool(true);

    let fields = UserFields {
        is_admin: Some(false),
        settings: Some(user_settings),
        roles: Some(vec!["user".to_string()]),
        ..Default::default()
    };
    db_save_user_row(&db.conn, username, &fields).await?;
    info!("Created user '{username}'.");
    get_user(db, username)
        .await?
        .ok_or_else(|| UserError::NotFound(format!("User '{username}' not found.")))
}

/// Create an empty group definition if it doesn't exist yet. Returns true if
/// it was created, false if it already existed (or the name was empty).
pub async fn ensure_group(db: &Db, group_name: &str) -> Result<bool, UserError> {
    if group_name.is_empty() {
        return Ok(false);
    }
    if get_group(db, group_name).await?.is_some() {
        return Ok(false);
    }
    let empty: Vec<String> = Vec::new();
    db_save_group(&db.conn, group_name, &serde_json::json!({}), Some(&empty), Some(&empty)).await?;
    info!("Created group definition '{group_name}'.");
    Ok(true)
}

/// Associate a user with a group. The group is added to the user's multi-group
/// membership list (which drives permission inheritance) and also recorded as
/// the user's *primary* group in `settings.group` (which drives quota/settings
/// overrides). Idempotent.
pub async fn set_user_group(db: &Db, username: &str, group_name: &str) -> Result<(), UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => return Ok(()),
    };
    let mut settings_dict = user
        .settings
        .clone()
        .unwrap_or_else(|| default_user_settings().clone());
    let mut groups = user.groups.clone();
    let current_group = settings_dict
        .get("group")
        .and_then(|v| v.as_str())
        .unwrap_or("none");
    if current_group == group_name && groups.iter().any(|g| g == group_name) {
        return Ok(());
    }
    ensure_group(db, group_name).await?;
    settings_dict["group"] = serde_json::Value::String(group_name.to_string());
    if !groups.iter().any(|g| g == group_name) {
        groups.push(group_name.to_string());
    }
    let fields = UserFields {
        settings: Some(settings_dict),
        groups: Some(groups),
        ..Default::default()
    };
    db_save_user_row(&db.conn, username, &fields).await?;
    info!("Set group '{group_name}' for user '{username}'.");
    Ok(())
}

/// Promote an existing user to admin. The user's per-user settings are
/// preserved; `active` is forced to True unless the account was explicitly set
/// to active=False. Idempotent: a no-op if already an admin.
pub async fn promote_user_to_admin(db: &Db, username: &str) -> Result<(), UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => return Err(UserError::NotFound(format!("User '{username}' not found."))),
    };
    if user.is_admin {
        return Ok(());
    }
    let mut roles = user.roles.clone();
    if !roles.iter().any(|r| r == "admin") {
        roles.push("admin".to_string());
    }
    let current = user.settings.clone().unwrap_or_default();
    let mut new_settings = default_user_settings().clone();
    new_settings = merge_settings(&new_settings, &current);
    // Force active=True unless it was explicitly disabled.
    if new_settings.get("active").and_then(|v| v.as_bool()) != Some(false) {
        new_settings["active"] = serde_json::Value::Bool(true);
    }
    let fields = UserFields {
        settings: Some(new_settings),
        roles: Some(roles),
        ..Default::default()
    };
    db_save_user_row(&db.conn, username, &fields).await?;
    recompute_is_admin(db, username).await?;
    info!("Promoted user '{username}' to admin.");
    Ok(())
}

/// Demote an existing admin to a regular user. The admin's per-user settings
/// are preserved; `active` is left untouched. The bootstrap account is
/// protected. Idempotent: a no-op if the account is not an admin.
pub async fn demote_admin_to_user(
    db: &Db,
    settings: &Settings,
    username: &str,
) -> Result<(), UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => return Err(UserError::NotFound(format!("User '{username}' not found."))),
    };
    if !user.is_admin {
        return Ok(());
    }
    let bootstrap = bootstrap_username(db, settings).await?;
    if username == bootstrap {
        return Err(UserError::Validation(format!(
            "Admin '{username}' is the bootstrap account and cannot be demoted."
        )));
    }
    let mut roles: Vec<String> = user
        .roles
        .iter()
        .filter(|r| r.as_str() != "admin")
        .cloned()
        .collect();
    if roles.is_empty() {
        roles = vec!["user".to_string()];
    }
    let current = user.settings.clone().unwrap_or_default();
    let new_settings = merge_settings(default_user_settings(), &current);
    let fields = UserFields {
        settings: Some(new_settings),
        roles: Some(roles),
        ..Default::default()
    };
    db_save_user_row(&db.conn, username, &fields).await?;
    recompute_is_admin(db, username).await?;
    info!("Demoted admin '{username}' to user.");
    Ok(())
}

/// Update a user's (or admin's) per-user settings. `ui_language` is a personal
/// preference set by the user's own language selector, not an admin-managed
/// capability; it is preserved when a roster edit omits it.
pub async fn update_user_settings(
    db: &Db,
    username: &str,
    new_settings: serde_json::Value,
) -> Result<(), UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => {
            return Err(UserError::NotFound(format!(
                "User or admin '{username}' not found."
            )))
        }
    };
    let mut new_settings = new_settings;
    if new_settings.get("ui_language").is_none() {
        if let Some(old) = user.settings.as_ref() {
            if let Some(lang) = old.get("ui_language") {
                if !lang.is_null() {
                    new_settings["ui_language"] = lang.clone();
                }
            }
        }
    }
    let fields = UserFields {
        settings: Some(new_settings),
        ..Default::default()
    };
    db_save_user_row(&db.conn, username, &fields).await?;
    info!("Updated settings for '{username}'.");
    Ok(())
}

/// Create or update a group and persist it. `roles` / `permissions` are
/// optional; when provided they replace the group's role/permission lists,
/// otherwise the existing values are kept.
pub async fn write_group_file(
    db: &Db,
    group_name: &str,
    settings_dict: serde_json::Value,
    roles: Option<Vec<String>>,
    permissions: Option<Vec<String>>,
) -> Result<(), UserError> {
    let existing = get_group(db, group_name).await?;
    let existing_roles = existing.as_ref().map(|g| g.roles.clone()).unwrap_or_default();
    let existing_permissions = existing
        .as_ref()
        .map(|g| g.permissions.clone())
        .unwrap_or_default();
    let roles = roles.unwrap_or(existing_roles);
    let permissions = permissions.unwrap_or(existing_permissions);
    db_save_group(&db.conn, group_name, &settings_dict, Some(&roles), Some(&permissions)).await?;
    info!("Wrote group '{group_name}'.");
    Ok(())
}

/// Delete a group and detach it from every user. Deleting a group also removes
/// it from each member's multi-group list and clears it as their primary group
/// (so its quota/settings overrides stop applying), then re-derives each
/// affected user's is_admin flag.
pub async fn delete_group(db: &Db, group_name: &str) -> Result<(), UserError> {
    if get_group(db, group_name).await?.is_none() {
        return Err(UserError::NotFound(format!(
            "Group '{group_name}' not found."
        )));
    }
    db_delete_group(&db.conn, group_name).await?;
    let users = entities::user::Entity::find().all(&db.conn).await?;
    for m in users {
        let mut groups = json_str_list(m.groups.clone());
        let mut settings_dict = m.settings.clone().unwrap_or_default();
        let mut changed = false;
        if groups.iter().any(|g| g == group_name) {
            groups.retain(|g| g != group_name);
            changed = true;
        }
        if settings_dict.get("group").and_then(|v| v.as_str()) == Some(group_name) {
            settings_dict["group"] = serde_json::Value::String("none".to_string());
            changed = true;
        }
        if changed {
            let fields = UserFields {
                groups: Some(groups),
                settings: Some(settings_dict),
                ..Default::default()
            };
            db_save_user_row(&db.conn, &m.username, &fields).await?;
            recompute_is_admin(db, &m.username).await?;
        }
    }
    info!("Deleted group '{group_name}'.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Home directories
// ---------------------------------------------------------------------------

/// List the home directories for a user (the subdirectories of the user's
/// storage path).
pub fn get_home_dirs(settings: &Settings, username: &str) -> Vec<String> {
    let user_storage_path = std::path::Path::new(&settings.storage_path).join(username);
    if !user_storage_path.is_dir() {
        return Vec::new();
    }
    match std::fs::read_dir(&user_storage_path) {
        Ok(entries) => {
            let mut dirs: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            dirs.sort();
            dirs
        }
        Err(e) => {
            tracing::error!("Error listing home directories for {username}: {e}");
            Vec::new()
        }
    }
}

/// Create a new home directory for a user (with a `Desktop/files` subdirectory).
pub fn create_home_dir(
    settings: &Settings,
    username: &str,
    home_name: &str,
) -> Result<(), UserError> {
    if !valid_name(home_name) {
        return Err(UserError::Validation(
            "Invalid home directory name. Use only letters, numbers, underscore, or hyphen."
                .into(),
        ));
    }
    let user_storage_path = std::path::Path::new(&settings.storage_path).join(username);
    let new_home_path = user_storage_path.join(home_name);
    if new_home_path.exists() {
        return Err(UserError::Validation(format!(
            "Home directory '{home_name}' already exists for user '{username}'."
        )));
    }
    std::fs::create_dir_all(new_home_path.join("Desktop").join("files")).map_err(|e| {
        tracing::error!("Failed to create home directory for {username}: {e}");
        UserError::Validation(format!(
            "Failed to create home directory for '{username}': {e}"
        ))
    })?;
    info!("Created home directory '{home_name}' for user '{username}'.");
    Ok(())
}

/// Delete a home directory for a user.
pub fn delete_home_dir(
    settings: &Settings,
    username: &str,
    home_name: &str,
) -> Result<(), UserError> {
    if !valid_name(home_name) {
        return Err(UserError::Validation("Invalid home directory name.".into()));
    }
    let home_path = std::path::Path::new(&settings.storage_path)
        .join(username)
        .join(home_name);
    if !home_path.is_dir() {
        return Err(UserError::Validation(format!(
            "Home directory '{home_name}' not found for user '{username}'."
        )));
    }
    std::fs::remove_dir_all(&home_path).map_err(|e| {
        tracing::error!("Failed to delete home directory for {username}: {e}");
        UserError::Validation(format!(
            "Failed to delete home directory for '{username}': {e}"
        ))
    })?;
    info!("Deleted home directory '{home_name}' for user '{username}'.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Password-based web login
//
// Web-login passwords are stored as an argon2id hash in the users table
// (password_hash column). This lets any user or admin get a web password
// without changing how the extension-based auth works.
// ---------------------------------------------------------------------------

/// Set (or overwrite) the web-login password for an existing user/admin.
pub async fn set_password(db: &Db, username: &str, password: &str) -> Result<(), UserError> {
    if !valid_name(username) {
        return Err(UserError::Validation(
            "Invalid username. Use only letters, numbers, underscore, or hyphen.".into(),
        ));
    }
    if get_user(db, username).await?.is_none() {
        return Err(UserError::NotFound(format!(
            "User or admin '{username}' not found."
        )));
    }
    if password.is_empty() || password.chars().count() < 8 {
        return Err(UserError::Validation(
            "Password must be at least 8 characters long.".into(),
        ));
    }
    let hashed = Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| UserError::Validation(format!("Failed to hash password: {e}")))?;
    db_set_password(&db.conn, username, Some(&hashed)).await?;
    info!("Set web-login password for '{username}'.");
    Ok(())
}

/// True if a web-login password is set for this user/admin.
pub async fn has_password(db: &Db, username: &str) -> Result<bool, UserError> {
    let user = get_user(db, username).await?;
    Ok(user.map(|u| u.password_hash.is_some()).unwrap_or(false))
}

/// True if the password matches. False if unset or wrong.
pub async fn verify_password(
    db: &Db,
    username: &str,
    password: &str,
) -> Result<bool, UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => return Ok(false),
    };
    let stored_hash = match user.password_hash {
        Some(h) if !h.is_empty() => h,
        _ => return Ok(false),
    };
    let parsed = match PasswordHash::new(&stored_hash) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Failed to parse stored hash for '{username}': {e}");
            return Ok(false);
        }
    };
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(e) => {
            tracing::error!("Failed to verify password for '{username}': {e}");
            Ok(false)
        }
    }
}

/// Hash a public-share password for storage. Uses argon2id (memory-hard,
/// resists offline brute force); falls back to a SHA-256 hex digest if the
/// argon2 dependency is missing. The verifier selects the comparison based on
/// the stored value's format.
pub fn hash_share_password(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .unwrap_or_else(|_| {
            let mut hasher = Sha256::new();
            hasher.update(password.as_bytes());
            hex::encode(hasher.finalize())
        })
}

/// Constant-time byte comparison (avoids timing attacks on the legacy
/// SHA-256 share-password path).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Verify a public-share password against its stored hash. New shares are
/// stored as an argon2id hash; shares created before the switch are a bare
/// SHA-256 hex digest. The stored value's format selects the comparison, so
/// existing shares keep working without a migration.
pub fn verify_share_password(stored_hash: &str, password: &str) -> bool {
    if stored_hash.is_empty() {
        return false;
    }
    if stored_hash.starts_with("$argon2") {
        let parsed = match PasswordHash::new(stored_hash) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Failed to verify share password: {e}");
                return false;
            }
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    } else {
        // Legacy SHA-256 hex digest.
        let mut hasher = Sha256::new();
        hasher.update(password.as_bytes());
        let submitted = hex::encode(hasher.finalize());
        constant_time_eq(submitted.as_bytes(), stored_hash.as_bytes())
    }
}

/// Remove the web-login password for a user/admin, if present.
pub async fn delete_password(db: &Db, username: &str) -> Result<(), UserError> {
    db_set_password(&db.conn, username, None).await?;
    info!("Deleted web-login password for '{username}'.");
    Ok(())
}

// ---------------------------------------------------------------------------
// SSO / OIDC
// ---------------------------------------------------------------------------

/// Flag a user as SSO/OIDC-provisioned. SSO users authenticate through the
/// identity provider, so their password is managed externally. Idempotent.
pub async fn mark_user_sso(db: &Db, username: &str) -> Result<(), UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => return Ok(()),
    };
    if user.is_sso {
        return Ok(());
    }
    let fields = UserFields {
        is_sso: Some(true),
        ..Default::default()
    };
    db_save_user_row(&db.conn, username, &fields).await?;
    info!("Marked '{username}' as an SSO-provisioned user.");
    Ok(())
}

/// Record the full list of provider (IdP) groups a user belongs to. Called on
/// every SSO login with the groups from the id_token. Idempotent: a no-op if
/// the list is unchanged.
pub async fn set_sso_groups(
    db: &Db,
    username: &str,
    groups: &[String],
) -> Result<(), UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => return Ok(()),
    };
    let groups: Vec<String> = groups
        .iter()
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .collect();
    if user.sso_groups == groups {
        return Ok(());
    }
    let fields = UserFields {
        sso_groups: Some(groups.clone()),
        ..Default::default()
    };
    db_save_user_row(&db.conn, username, &fields).await?;
    info!("Recorded {} SSO group(s) for '{username}'.", groups.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Role management (RBAC)
//
// Roles are named bundles of permissions that can be assigned to users and to
// groups. The built-in roles (admin / operator / user) are seeded on first
// run but are fully editable; custom roles can be created and deleted.
// ---------------------------------------------------------------------------

/// Create a new (custom) role. The name must be unique and valid.
pub async fn create_role(
    db: &Db,
    name: &str,
    description: &str,
    permissions: &[String],
) -> Result<RoleRecord, UserError> {
    if !valid_name(name) {
        return Err(UserError::Validation(
            "Invalid role name. Use only letters, numbers, underscore, or hyphen.".into(),
        ));
    }
    if get_role(db, name).await?.is_some() {
        return Err(UserError::Validation(format!("Role '{name}' already exists.")));
    }
    for p in permissions {
        if !permissions::is_valid_permission(p) {
            return Err(UserError::Validation(format!("Unknown permission '{p}'.")));
        }
    }
    db_save_role(&db.conn, name, description, permissions, false).await?;
    info!("Created role '{name}'.");
    get_role(db, name)
        .await?
        .ok_or_else(|| UserError::NotFound(format!("Role '{name}' not found.")))
}

/// Update a role's description and/or permissions (built-ins included).
/// Changing a role's permissions changes the effective permissions of every
/// user (directly or via a group) that carries it, so all users' is_admin
/// flags are re-derived afterwards.
pub async fn update_role(
    db: &Db,
    name: &str,
    description: Option<&str>,
    permissions: Option<&[String]>,
) -> Result<RoleRecord, UserError> {
    let role = match get_role(db, name).await? {
        Some(r) => r,
        None => return Err(UserError::NotFound(format!("Role '{name}' not found."))),
    };
    let mut new_description = role.description.clone();
    let mut new_permissions = role.permissions.clone();
    if let Some(d) = description {
        new_description = d.to_string();
    }
    if let Some(p) = permissions {
        for perm in p {
            if !permissions::is_valid_permission(perm) {
                return Err(UserError::Validation(format!("Unknown permission '{perm}'.")));
            }
        }
        new_permissions = p.to_vec();
    }
    db_save_role(&db.conn, name, &new_description, &new_permissions, role.is_builtin).await?;
    // Re-derive is_admin for every user (a role change affects everyone who
    // carries it, directly or via a group).
    let users = entities::user::Entity::find().all(&db.conn).await?;
    for m in users {
        recompute_is_admin(db, &m.username).await?;
    }
    info!("Updated role '{name}'.");
    get_role(db, name)
        .await?
        .ok_or_else(|| UserError::NotFound(format!("Role '{name}' not found.")))
}

/// Delete a role. Refused for built-in roles, or while any user or group still
/// carries it.
pub async fn delete_role(db: &Db, name: &str) -> Result<(), UserError> {
    let role = match get_role(db, name).await? {
        Some(r) => r,
        None => return Err(UserError::NotFound(format!("Role '{name}' not found."))),
    };
    if role.is_builtin {
        return Err(UserError::Validation(format!(
            "Role '{name}' is a built-in role and cannot be deleted."
        )));
    }
    let mut used_by: Vec<String> = Vec::new();
    let users = entities::user::Entity::find().all(&db.conn).await?;
    for m in users {
        if json_str_list(m.roles.clone()).iter().any(|r| r == name) {
            used_by.push(m.username.clone());
        }
    }
    let groups = entities::group::Entity::find().all(&db.conn).await?;
    for g in groups {
        if json_str_list(g.roles.clone()).iter().any(|r| r == name) {
            used_by.push(format!("group:{}", g.name));
        }
    }
    if !used_by.is_empty() {
        return Err(UserError::Validation(format!(
            "Role '{name}' is still assigned to: {}. Remove it from those accounts first.",
            used_by.join(", ")
        )));
    }
    db_delete_role(&db.conn, name).await?;
    info!("Deleted role '{name}'.");
    Ok(())
}

/// Set a user's role/permission/group assignments (partial update). Each
/// provided argument replaces the corresponding list; omitted arguments are
/// left unchanged. When `groups` is provided, its first entry becomes the
/// user's primary group (`settings.group`), which drives quota and settings
/// overrides. After the update the user's is_admin flag is re-derived from the
/// resulting effective permissions.
pub async fn set_user_access(
    db: &Db,
    settings: &Settings,
    username: &str,
    roles: Option<Vec<String>>,
    permissions: Option<Vec<String>>,
    groups: Option<Vec<String>>,
) -> Result<UserRecord, UserError> {
    let user = match get_user(db, username).await? {
        Some(u) => u,
        None => {
            return Err(UserError::NotFound(format!(
                "User or admin '{username}' not found."
            )))
        }
    };

    // Validate everything and resolve the resulting roles BEFORE mutating any
    // state, so a rejected request never leaves the account half-updated.
    let resulting_roles = match &roles {
        Some(r) => r.clone(),
        None => user.roles.clone(),
    };
    if let Some(r) = &roles {
        for role in r {
            if get_role(db, role).await?.is_none() {
                return Err(UserError::Validation(format!("Unknown role '{role}'.")));
            }
        }
    }
    if let Some(p) = &permissions {
        for perm in p {
            if !permissions::is_valid_permission(perm) {
                return Err(UserError::Validation(format!("Unknown permission '{perm}'.")));
            }
        }
    }
    // The bootstrap account must always keep the admin role, or everyone would
    // be locked out of the admin UI.
    let bootstrap = bootstrap_username(db, settings).await?;
    if username == bootstrap && !resulting_roles.iter().any(|r| r == "admin") {
        return Err(UserError::Validation(format!(
            "The root '{username}' account must keep the 'admin' role."
        )));
    }

    let mut fields = UserFields::default();
    if let Some(r) = &roles {
        fields.roles = Some(r.clone());
    }
    if let Some(p) = &permissions {
        fields.permissions = Some(p.clone());
    }
    if let Some(g) = &groups {
        // Ensure each group exists (skip empty / "none").
        for gname in g {
            if !gname.is_empty() && gname != "none" {
                ensure_group(db, gname).await?;
            }
        }
        let new_groups: Vec<String> = g
            .iter()
            .filter(|g| !g.is_empty() && g.as_str() != "none")
            .cloned()
            .collect();
        fields.groups = Some(new_groups.clone());
        // The first group becomes the primary group (drives quota/settings).
        let mut new_settings = user
            .settings
            .clone()
            .unwrap_or_else(|| default_user_settings().clone());
        new_settings["group"] = serde_json::Value::String(
            new_groups
                .first()
                .cloned()
                .unwrap_or_else(|| "none".to_string()),
        );
        fields.settings = Some(new_settings);
    }
    db_save_user_row(&db.conn, username, &fields).await?;
    recompute_is_admin(db, username).await?;
    info!("Updated access for '{username}'.");
    get_user(db, username)
        .await?
        .ok_or_else(|| UserError::NotFound(format!("User or admin '{username}' not found.")))
}
