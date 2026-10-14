//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Configuration database layer (SeaORM).
//!
//! Ports `server/db`. All durable application configuration (users, groups,
//! volume mounts, app stores, installed apps, app templates, public shares,
//! plus the ephemeral session and auth-token stores) is persisted here instead
//! of the legacy YAML files. The backend is selected by the `db_*` settings
//! (see `Settings::build_database_url`): SQLite by default, Postgres/MySQL
//! optional. The schema and queries are identical across backends.
//!
//! Unlike the Python code (which used a `NullPool` so the engine was safe to
//! drive from multiple event loops), this is a single tokio runtime, so a real
//! connection pool is safe and much faster: up to 10 connections, with a cheap
//! idle-timeout health check instead of pinging on every acquire.
//!
//! The schema is defined once by the SeaORM entities in `entities.rs` and
//! created/synced by [`Db::init`], which both creates missing tables and adds
//! missing columns to existing ones (mirroring the Python `init_db` +
//! `migrate_db` pair).

use std::time::Duration;

use log::LevelFilter;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict, Query};
use sea_orm::{
    ActiveValue, ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend,
    EntityTrait, QueryResult, QueryTrait, Statement, TransactionTrait, Value,
};
use tracing::info;

use crate::entities;

/// Tables in a sensible restore order. No foreign-key constraints are declared
/// (cross-references are handled in application code), so the order is only for
/// readability of the logical backup.
pub const TABLE_ORDER: [&str; 13] = [
    "users",
    "groups",
    "roles",
    "volume_mounts",
    "app_stores",
    "installed_apps",
    "app_templates",
    "template_schema_settings",
    "pinned_behaviors",
    "app_settings",
    "public_shares",
    "sessions",
    "auth_tokens",
];

/// The configuration database handle.
///
/// `DatabaseConnection` is cheap to clone (it wraps a shared pool), so this can
/// be cloned into `AppState` and handed to every handler.
#[derive(Clone)]
pub struct Db {
    pub conn: DatabaseConnection,
    /// The backend dialect name: "sqlite", "postgres", or "mysql".
    pub dialect: String,
}

/// Detect the backend from a sqlx URL scheme.
fn dialect_from_url(url: &str) -> String {
    if url.starts_with("sqlite") {
        "sqlite".to_string()
    } else if url.starts_with("postgres") {
        "postgres".to_string()
    } else if url.starts_with("mysql") || url.starts_with("mariadb") {
        "mysql".to_string()
    } else {
        "sqlite".to_string()
    }
}

/// Connect to the configuration database.
///
/// `url` should be a sqlx-format URL (see `Settings::build_database_url`). A
/// real connection pool is used: up to 10 connections, at least 1 kept warm,
/// with a health check only for connections that have been idle for a while
/// (cheaper than pinging on every acquire).
pub async fn connect(url: &str) -> Result<Db, sea_orm::DbErr> {
    let dialect = dialect_from_url(url);
    let mut opt = ConnectOptions::new(url);
    opt.max_connections(10);
    opt.min_connections(1);
    opt.test_before_acquire_if_idle_for(Duration::from_secs(30));
    // sea-orm's `ConnectOptions::new` defaults SQL statement logging to
    // `Info`, which floods the log with every query. Drop it to `Debug` so the
    // per-query lines only appear when the global log level is `debug` (or
    // higher), matching the Python server's quieter default behaviour.
    opt.sqlx_logging_level(LevelFilter::Debug);

    let conn = Database::connect(opt).await?;

    // Enable WAL on SQLite for better concurrent read/write behaviour.
    // Non-fatal: if the pragma fails we still have a working database.
    if dialect == "sqlite" {
        let backend = conn.get_database_backend();
        let _ = conn
            .execute_raw(Statement::from_string(backend, "PRAGMA journal_mode=WAL;"))
            .await;
    }

    info!("Configuration database ready ({dialect})");
    Ok(Db { conn, dialect })
}

impl Db {
    /// Create any missing tables and add any missing columns (idempotent).
    ///
    /// Uses SeaORM's schema builder in "sync" mode: it creates tables that
    /// don't exist yet and adds columns that are missing from existing tables,
    /// but never drops anything. This subsumes the Python `init_db` +
    /// `migrate_db` pair. Each entity's column defaults (see `entities.rs`)
    /// are applied when a column is added, matching the Python migrations.
    pub async fn init(&self) -> Result<(), sea_orm::DbErr> {
        self.conn
            .get_schema_builder()
            .register(entities::user::Entity)
            .register(entities::group::Entity)
            .register(entities::role::Entity)
            .register(entities::volume_mount::Entity)
            .register(entities::app_store::Entity)
            .register(entities::installed_app::Entity)
            .register(entities::session_record::Entity)
            .register(entities::auth_token::Entity)
            .register(entities::public_share::Entity)
            .register(entities::app_template::Entity)
            .register(entities::template_schema_setting::Entity)
            .register(entities::pinned_behavior::Entity)
            .register(entities::app_setting::Entity)
            .sync(&self.conn)
            .await?;
        info!("Configuration database schema synced");
        Ok(())
    }

    /// Read a single value from the `app_settings` key/value store (None if the
    /// key is absent). Used for settings that are edited at runtime (e.g. the
    /// OIDC admin-groups list) rather than sourced from the environment.
    pub async fn get_app_setting(&self, key: &str) -> Result<Option<String>, sea_orm::DbErr> {
        let row = entities::app_setting::Entity::find_by_id(key.to_string())
            .one(&self.conn)
            .await?;
        Ok(row.map(|r| r.value))
    }


    /// Insert or update a single `app_settings` row.
    pub async fn set_app_setting(&self, key: &str, value: &str) -> Result<(), sea_orm::DbErr> {
        let am = entities::app_setting::ActiveModel {
            key: ActiveValue::Set(key.to_string()),
            value: ActiveValue::Set(value.to_string()),
        };
        // Upsert: insert, or update the existing row if the key is present.
        let stmt = entities::app_setting::Entity::insert(am)
            .on_conflict(
                OnConflict::column(entities::app_setting::Column::Key)
                    .update_column(entities::app_setting::Column::Value)
                    .to_owned(),
            )
            .build(self.conn.get_database_backend());
        self.conn.execute_raw(stmt).await?;
        Ok(())
    }

    /// Write a logical, backend-agnostic snapshot of every table to a JSON file.
    ///
    /// The format is `{"version": 1, "tables": {"users": [ {...}, ... ], ...}}`
    /// which lets the same backup be restored onto SQLite, Postgres, or MySQL.
    ///
    /// The `auth_tokens` table is excluded: its rows are live session tokens
    /// held as the plaintext primary key, and active tokens are intentionally
    /// not part of a backup (a restore leaves in-flight sessions untouched).
    /// The dump file is written `0600` (owner read/write only) because
    /// `users.password_hash` is included.
    pub async fn dump(&self, dest_path: &str) -> Result<(), sea_orm::DbErr> {
        let backend = self.conn.get_database_backend();
        let mut tables = serde_json::Map::new();
        for table in TABLE_ORDER {
            if table == "auth_tokens" {
                continue;
            }
            let rows = self
                .conn
                .query_all_raw(Statement::from_string(backend, &format!("SELECT * FROM {table}")))
                .await?;
            let mut out = Vec::new();
            for row in &rows {
                out.push(row_to_json_object(row));
            }
            tables.insert(table.to_string(), serde_json::Value::Array(out));
        }
        let data = serde_json::json!({ "version": 1, "tables": tables });
        std::fs::write(dest_path, data.to_string())
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
        let _ = std::fs::set_permissions(
            dest_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
        info!("Wrote configuration database snapshot to {dest_path}");
        Ok(())
    }

    /// Replace the contents of every table with the rows in a [`Db::dump`] file.
    ///
    /// Runs inside a single transaction so concurrent readers observe either the
    /// old or the new state, never a partially-restored one. Unknown columns in
    /// the dump (e.g. from a different schema version) are ignored.
    pub async fn restore(&self, source_path: &str) -> Result<(), sea_orm::DbErr> {
        let raw = std::fs::read_to_string(source_path)
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
        let data: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
        let tables = data.get("tables").cloned().unwrap_or(serde_json::json!({}));

        let backend = self.conn.get_database_backend();
        let txn = self.conn.begin().await?;
        for table in TABLE_ORDER {
            let has_entry = tables.get(table).is_some();
            // New-style dumps exclude `auth_tokens` (live session tokens); a
            // missing entry means the live token table must be left untouched.
            // Older dumps always carry the key, restoring the snapshot as-is.
            if table == "auth_tokens" && !has_entry {
                continue;
            }
            txn.execute_raw(Statement::from_string(backend, &format!("DELETE FROM {table}")))
                .await?;
            if let Some(rows) = tables.get(table).and_then(|v| v.as_array()) {
                for row in rows {
                    insert_json_row(&txn, backend, table, row).await?;
                }
            }
        }
        txn.commit().await?;
        info!("Restored configuration database from {source_path}");
        Ok(())
    }
}

/// The known columns of each table (in entity order). Used by [`restore`] to
/// ignore columns that aren't part of the current schema.
fn known_columns(table: &str) -> Vec<&'static str> {
    match table {
        "users" => vec![
            "username",
            "is_admin",
            "is_sso",
            "settings",
            "password_hash",
            "must_change_password",
            "sso_groups",
            "roles",
            "permissions",
            "groups",
        ],
        "groups" => vec!["name", "settings", "roles", "permissions"],
        "roles" => vec!["name", "description", "permissions", "is_builtin"],
        "volume_mounts" => vec![
            "name",
            "host_path",
            "container_path",
            "read_only",
            "scope",
            "target",
        ],
        "app_stores" => vec!["name", "url"],
        "installed_apps" => vec!["id", "data"],
        "sessions" => vec!["session_id", "data"],
        "auth_tokens" => vec!["token", "username", "expires_at"],
        "public_shares" => vec!["share_id", "data"],
        "app_templates" => vec!["name", "settings"],
        "template_schema_settings" => vec![
            "name",
            "sort_order",
            "label",
            "description",
            "category",
            "type",
            "default",
            "current",
            "docker",
            "options",
        ],
        "pinned_behaviors" => vec!["id", "username", "created_at", "data"],
        "app_settings" => vec!["key", "value"],
        _ => vec![],
    }
}

/// Convert a raw query row into a JSON object using the column names.
///
/// Each cell is extracted as a `serde_json::Value`. On Postgres, JSON columns
/// come back as native JSON and are read directly; on SQLite/MySQL the same
/// columns are stored as TEXT, so we read them as a string and parse them back
/// into JSON when they look like an object or array. This keeps the dump format
/// identical across backends.
fn row_to_json_object(row: &QueryResult) -> serde_json::Value {
    let names = row.column_names();
    let mut obj = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        obj.insert(name.clone(), extract_cell(row, i));
    }
    serde_json::Value::Object(obj)
}

/// Extract a single cell as a `serde_json::Value`, trying the native JSON type
/// first (Postgres) and falling back to a string parse (SQLite/MySQL).
fn extract_cell(row: &QueryResult, idx: usize) -> serde_json::Value {
    if let Ok(v) = row.try_get_by_index::<serde_json::Value>(idx) {
        return v;
    }
    if let Ok(s) = row.try_get_by_index::<String>(idx) {
        let t = s.trim();
        if t.starts_with('{') || t.starts_with('[') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                return v;
            }
        }
        return serde_json::Value::String(s);
    }
    if let Ok(b) = row.try_get_by_index::<bool>(idx) {
        return serde_json::Value::Bool(b);
    }
    if let Ok(i) = row.try_get_by_index::<i64>(idx) {
        return serde_json::json!(i);
    }
    if let Ok(f) = row.try_get_by_index::<f64>(idx) {
        return serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
    }
    serde_json::Value::Null
}

/// Convert a dumped JSON value into a SeaORM `Value` for re-insertion.
///
/// Objects and arrays are JSON columns and are stored as a JSON value; scalars
/// map to their native column types. (In this schema JSON columns always hold
/// objects or arrays, never bare scalars, so this heuristic is unambiguous.)
fn json_to_sea_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::String(None),
        serde_json::Value::String(s) => Value::String(Some(s.clone())),
        serde_json::Value::Bool(b) => Value::Bool(Some(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::BigInt(Some(i))
            } else {
                Value::Double(Some(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            Value::Json(Some(Box::new(v.clone())))
        }
    }
}

/// Insert a single dumped JSON row into a table, binding only the columns that
/// exist in the current schema (unknown columns are ignored).
async fn insert_json_row(
    exec: &impl ConnectionTrait,
    backend: DbBackend,
    table: &str,
    row: &serde_json::Value,
) -> Result<(), sea_orm::DbErr> {
    let obj = match row.as_object() {
        Some(o) => o,
        None => return Ok(()),
    };
    let json_cols = json_columns(table);
    let mut col_names = Vec::new();
    let mut values = Vec::new();
    for col in known_columns(table) {
        if let Some(v) = obj.get(col) {
            col_names.push(col.to_string());
            // On Postgres the JSON columns are typed `json`. A `Value::Json`
            // param is mis-encoded by the driver (and a NULL string param is
            // typed `text`), both of which Postgres rejects when assigned to a
            // `json` column. Instead, serialize the value to a JSON string and
            // cast it to `json`: a plain string param encodes as text, and
            // `CAST(text AS json)` type-checks. On SQLite/MySQL the same columns
            // are TEXT, so the value is inserted as-is (no cast).
            let expr = if matches!(backend, DbBackend::Postgres) && json_cols.contains(&col) {
                Expr::val(v.to_string()).cast_as("json")
            } else {
                Expr::val(json_to_sea_value(v))
            };
            values.push(expr);
        }
    }
    if col_names.is_empty() {
        return Ok(());
    }
    let mut insert = Query::insert();
    insert.into_table(table.to_string());
    insert.columns(col_names.iter().cloned());
    insert.values_panic(values);
    let stmt = backend.build(&insert);
    exec.execute_raw(stmt).await?;
    Ok(())
}

/// The JSON-typed columns of each table (mirrors the `Json` fields in
/// `entities.rs`). On Postgres these are `json` columns that need an explicit
/// cast when re-inserting dumped rows; on SQLite/MySQL they are TEXT.
fn json_columns(table: &str) -> Vec<&'static str> {
    match table {
        "users" => vec!["settings", "sso_groups", "roles", "permissions", "groups"],
        "groups" => vec!["settings", "roles", "permissions"],
        "roles" => vec!["permissions"],
        "installed_apps" => vec!["data"],
        "sessions" => vec!["data"],
        "public_shares" => vec!["data"],
        "app_templates" => vec!["settings"],
        "template_schema_settings" => vec!["options"],
        "pinned_behaviors" => vec!["data"],
        _ => vec![],
    }
}
