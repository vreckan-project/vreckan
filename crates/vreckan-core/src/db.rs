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

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use log::LevelFilter;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict, Query};
use sea_orm::{
    ActiveValue, ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend,
    EntityTrait, QueryResult, QueryTrait, Statement, TransactionTrait, Value,
};
use tracing::{info, warn};

use crate::entities;

/// Tables in a sensible restore order. No foreign-key constraints are declared
/// (cross-references are handled in application code), so the order is only for
/// readability of the logical backup.
pub const TABLE_ORDER: [&str; 14] = [
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
    "session_shares",
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
    /// Integration-test hook: when `true`, writes report a simulated outage
    /// (lets tests exercise the DB-error paths without a second backend).
    /// Production code leaves it `false`.
    pub fail_writes: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Symmetric read hook for `fail_writes`: when `true`, the durable-store
    /// loaders report a simulated outage before issuing their SELECT, so
    /// tests can exercise the fatal-load paths without a second backend.
    /// Production code leaves it `false`.
    pub fail_reads: std::sync::Arc<std::sync::atomic::AtomicBool>,
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

/// Extract the database file path from a sqlx SQLite URL
/// (`sqlite://<path>?<query>`), e.g. `sqlite:///data/vreckan.db?mode=rwc`.
/// Returns `None` for in-memory databases and non-SQLite URLs.
pub(crate) fn sqlite_file_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("sqlite://")?;
    let path = rest.split('?').next()?.trim();
    if path.is_empty() || path == ":memory:" {
        return None;
    }
    Some(path.to_string())
}

/// Best-effort tightening of a SQLite database file and its `-wal`/`-shm`
/// sidecars to owner-only (`0600`). The configuration database holds users,
/// sessions, and auth tokens, and the driver creates the file with the
/// process umask, so on a multi-user host other local users could read it
/// straight off disk (ANT-345). Missing sidecars are expected — SQLite
/// creates them as needed — and other errors are logged rather than
/// propagated, so a non-writable directory cannot fail boot.
pub(crate) fn tighten_sqlite_file(path: &str) {
    for suffix in ["", "-wal", "-shm"] {
        let file = format!("{path}{suffix}");
        let meta = match std::fs::metadata(&file) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warn!(
                    "could not stat SQLite file {file}: {e} (leaving permissions as-is)"
                );
                continue;
            }
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode == 0o600 {
            continue;
        }
        match std::fs::set_permissions(
            &file,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        ) {
            Ok(()) => info!("tightened SQLite file to 0600: {file}"),
            Err(e) => warn!("could not tighten SQLite file to 0600: {file}: {e}"),
        }
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
        // Tighten the database file and WAL/SHM sidecars to owner-only,
        // after the WAL pragma has created the sidecars (ANT-345).
        // Best effort: see `tighten_sqlite_file`.
        if let Some(path) = sqlite_file_path(url) {
            tighten_sqlite_file(&path);
        }
    }

    info!("Configuration database ready ({dialect})");
    Ok(Db {
        conn,
        dialect,
        fail_writes: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        fail_reads: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    })
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
            .register(entities::session_share::Entity)
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
        if self.fail_writes.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Self::simulated_write_failure());
        }
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

    /// A simulated-outage error for the `fail_writes` test hook, shared by
    /// `set_app_setting` and the transactional store saves (which check it at
    /// commit time, i.e. after their statements have run inside the open
    /// transaction, so the failed save rolls the whole rewrite back).
    pub fn simulated_write_failure() -> sea_orm::DbErr {
        sea_orm::DbErr::Custom("simulated write failure (test hook)".to_string())
    }

    /// `Some` with a simulated-outage error when the `fail_reads` test hook is
    /// armed; the durable-store loaders consult this before their SELECT so
    /// tests can exercise the fatal-load paths without a second backend.
    pub fn fail_reads_error(&self) -> Option<sea_orm::DbErr> {
        self.fail_reads
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(|| {
                sea_orm::DbErr::Custom("simulated read failure (test hook)".to_string())
            })
    }

    /// The placeholder `dump()` writes into the `value` column of a secret
    /// `app_settings` row instead of the real value.
    pub const REDACTED_VALUE: &str = "<redacted>";

    /// `app_settings` rows whose value is a secret and must not ride in
    /// plaintext in a backup archive (ANT-273): the live ACME TLS private
    /// key, the Cloudflare Zone.DNS token, the generic DNS-provider
    /// credential JSON, and the OIDC client secret. Both the env-var form
    /// (`VRECKAN_*`, written by the admin pages and the ACME persistence
    /// path) and the raw setting-name form (legacy or foreign rows) are
    /// covered.
    pub const SECRET_APP_SETTING_KEYS: &[&str] = &[
        "VRECKAN_ACME_KEY_PEM",
        "VRECKAN_ACME_CLOUDFLARE_TOKEN",
        "VRECKAN_ACME_DNS_PROVIDER_CONFIG",
        "VRECKAN_OIDC_CLIENT_SECRET",
        "acme_cloudflare_token",
        "acme_dns_provider_config",
        "oidc_client_secret",
    ];

    /// Write a logical, backend-agnostic snapshot of every table to a JSON file.
    ///
    /// The format is `{"version": 1, "tables": {"users": [ {...}, ... ], ...}}`
    /// which lets the same backup be restored onto SQLite, Postgres, or MySQL.
    ///
    /// Every table is read inside one transaction, so the archive is a single
    /// point-in-time snapshot: a concurrent multi-table write (e.g. a
    /// `save_sessions` rewrite racing the dump) cannot split across the
    /// tables and leave the archive holding a combination of rows that never
    /// coexisted (audit finding 17). Postgres pins the transaction to
    /// REPEATABLE READ because its default READ COMMITTED takes a fresh
    /// snapshot per statement; SQLite read snapshots already span the
    /// transaction, and MySQL's InnoDB default is already REPEATABLE READ.
    ///
    /// The `auth_tokens` table is excluded: its rows are live session tokens
    /// held as the plaintext primary key, and active tokens are intentionally
    /// not part of a backup (a restore leaves in-flight sessions untouched).
    /// The dump file is written `0600` (owner read/write only) because
    /// `users.password_hash` is included.
    ///
    /// Secret `app_settings` rows (see [`Db::SECRET_APP_SETTING_KEYS`]) are
    /// redacted to [`Db::REDACTED_VALUE`] so an archive copied to another
    /// host or object store does not carry the ACME TLS private key or the
    /// DNS/OIDC provider credentials in plaintext (ANT-273).
    pub async fn dump(&self, dest_path: &str) -> Result<(), sea_orm::DbErr> {
        let backend = self.conn.get_database_backend();
        let txn = self.conn.begin().await?;
        // Must be the first statement of the transaction (Postgres rule); the
        // level resets to the session default at commit/rollback, so the
        // pooled connection carries no state into the next user.
        if backend == DbBackend::Postgres {
            txn
                .execute_raw(Statement::from_string(
                    backend,
                    "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                ))
                .await?;
        }
        let mut tables = serde_json::Map::new();
        for table in TABLE_ORDER {
            if table == "auth_tokens" {
                continue;
            }
            let rows = txn
                .query_all_raw(Statement::from_string(backend, format!("SELECT * FROM {table}")))
                .await?;
            let mut out = Vec::new();
            for row in &rows {
                let mut obj = row_to_json_object(row);
                if table == "app_settings" {
                    let is_secret = obj
                        .get("key")
                        .and_then(|k| k.as_str())
                        .is_some_and(is_secret_app_setting);
                    if is_secret {
                        // The value may have been re-parsed into a JSON object
                        // or array (see `extract_cell`), so replace the whole
                        // field rather than just a string cell.
                        if let serde_json::Value::Object(map) = &mut obj {
                            map.insert(
                                "value".to_string(),
                                serde_json::Value::String(Self::REDACTED_VALUE.to_string()),
                            );
                        }
                    }
                }
                out.push(obj);
            }
            tables.insert(table.to_string(), serde_json::Value::Array(out));
        }
        // Commit before writing the file so a failed dump leaves no file
        // behind and the snapshot transaction is released either way.
        txn.commit().await?;
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
    ///
    /// Before the transaction opens, every table in [`TABLE_ORDER`] must have a
    /// valid JSON-array entry in the dump, or the restore aborts with the
    /// offending table names: a table that exists in the live schema but is
    /// absent from (or malformed in) the dump — an older build's archive or a
    /// hand-trimmed one — would otherwise be `DELETE`d and left empty while the
    /// restore still reports success (audit finding 3). The one exemption is
    /// `auth_tokens` with no entry at all: new-style dumps exclude it (live
    /// session tokens), so a missing entry means the live token table is left
    /// untouched (logged); older dumps always carry the key and restore the
    /// snapshot as-is.
    ///
    /// A secret `app_settings` row whose dumped value is [`Db::REDACTED_VALUE`]
    /// is skipped: the archive only records that the key existed, so the live
    /// value must be re-entered after restore. Older dumps that carry the
    /// plaintext value restore it as-is (ANT-273).
    pub async fn restore(&self, source_path: &str) -> Result<(), sea_orm::DbErr> {
        let raw = std::fs::read_to_string(source_path)
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
        let data: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
        let tables = data.get("tables").cloned().unwrap_or(serde_json::json!({}));

        // Coverage check: every live table needs a valid array entry before any
        // DELETE runs, so an older or trimmed dump cannot silently empty tables
        // the admin never intended to touch.
        let mut incomplete = Vec::new();
        for table in TABLE_ORDER {
            match tables.get(table) {
                Some(v) if v.as_array().is_some() => {}
                None if table == "auth_tokens" => {
                    info!(
                        "Restore from {source_path}: no auth_tokens entry; \
                         leaving the live token table untouched."
                    );
                }
                _ => incomplete.push(table),
            }
        }
        if !incomplete.is_empty() {
            return Err(sea_orm::DbErr::Custom(format!(
                "restore aborted before any table was modified: the dump has no \
                 valid array entry for table(s) {}; restoring would empty them \
                 without re-populating",
                incomplete.join(", ")
            )));
        }

        let backend = self.conn.get_database_backend();
        let txn = self.conn.begin().await?;
        for table in TABLE_ORDER {
            // Reached only because the coverage check above passed: every table
            // has an array entry except `auth_tokens`, which is skipped when
            // the dump carries no entry for it (live-token semantics).
            if table == "auth_tokens" && tables.get(table).is_none() {
                continue;
            }
            txn.execute_raw(Statement::from_string(backend, format!("DELETE FROM {table}")))
                .await?;
            // The coverage check above guarantees an array entry here (the
            // entry-less `auth_tokens` exemption is skipped above).
            if let Some(rows) = tables.get(table).and_then(|v| v.as_array()) {
                for row in rows {
                    if table == "app_settings"
                        && row.get("value").and_then(|v| v.as_str()) == Some(Self::REDACTED_VALUE)
                        && row
                            .get("key")
                            .and_then(|k| k.as_str())
                            .is_some_and(is_secret_app_setting)
                    {
                        continue;
                    }
                    insert_json_row(&txn, backend, table, row).await?;
                }
            }
        }
        txn.commit().await?;
        info!("Restored configuration database from {source_path}");
        Ok(())
    }
}

/// Whether an `app_settings` row must be redacted in [`Db::dump`] (and skipped
/// in [`Db::restore`] when it carries [`Db::REDACTED_VALUE`]) so its value
/// does not ride plaintext in a backup archive (ANT-273).
pub fn is_secret_app_setting(key: &str) -> bool {
    Db::SECRET_APP_SETTING_KEYS.contains(&key)
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
        "session_shares" => vec!["share_id", "data"],
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
        "session_shares" => vec!["data"],
        "app_templates" => vec!["settings"],
        "template_schema_settings" => vec!["options"],
        "pinned_behaviors" => vec!["data"],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vreckan-db-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn sqlite_file_path_parses_sqlite_urls() {
        assert_eq!(
            sqlite_file_path("sqlite:///var/lib/vreckan/vreckan.db?mode=rwc"),
            Some("/var/lib/vreckan/vreckan.db".to_string())
        );
        assert_eq!(
            sqlite_file_path("sqlite:///data/vreckan.db"),
            Some("/data/vreckan.db".to_string())
        );
        assert_eq!(sqlite_file_path("sqlite://:memory:"), None);
        // A bare root path is pathological but well-formed; the tightening
        // stays best-effort if the location turns out to be unwritable.
        assert_eq!(sqlite_file_path("sqlite:///?mode=rwc"), Some("/".to_string()));
        assert_eq!(sqlite_file_path("postgres://u:p@h:5432/vreckan"), None);
    }

    #[test]
    fn tighten_sqlite_file_tightens_db_and_sidecars() {
        let dir = tmp_dir("tighten");
        let db = dir.join("vreckan.db");
        let wal = dir.join("vreckan.db-wal");
        let shm = dir.join("vreckan.db-shm");
        for p in [&db, &wal, &shm] {
            std::fs::write(p, "x").unwrap();
            std::fs::set_permissions(p, PermissionsExt::from_mode(0o644)).unwrap();
        }
        tighten_sqlite_file(db.to_str().unwrap());
        assert_eq!(mode_of(&db), 0o600, "db file must land 0600");
        assert_eq!(mode_of(&wal), 0o600, "wal sidecar must land 0600");
        assert_eq!(mode_of(&shm), 0o600, "shm sidecar must land 0600");
        // A second pass is a no-op (already 0600) and must not fail.
        tighten_sqlite_file(db.to_str().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tighten_sqlite_file_tolerates_missing_sidecars() {
        let dir = tmp_dir("tighten-missing");
        let db = dir.join("vreckan.db");
        std::fs::write(&db, "x").unwrap();
        std::fs::set_permissions(&db, PermissionsExt::from_mode(0o644)).unwrap();
        // -wal/-shm absent: the main file must still be tightened, without
        // treating the missing sidecars as errors.
        tighten_sqlite_file(db.to_str().unwrap());
        assert_eq!(mode_of(&db), 0o600, "db file must land 0600");
        assert!(!dir.join("vreckan.db-wal").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn connect_tightens_live_sqlite_db_to_0600() {
        let dir = tmp_dir("connect");
        let db = dir.join("live.db");
        // Seed a pre-existing database at umask permissions (the pre-fix
        // shape): connect must tighten it on boot.
        std::fs::write(&db, "").unwrap();
        std::fs::set_permissions(&db, PermissionsExt::from_mode(0o644)).unwrap();
        let d = connect(&format!("sqlite://{}?mode=rwc", db.display()))
            .await
            .unwrap();
        assert_eq!(d.dialect, "sqlite");
        assert_eq!(mode_of(&db), 0o600, "db file must land 0600");
        // The WAL pragma creates the sidecars; any that exist must be
        // tightened too.
        for p in [&dir.join("live.db-wal"), &dir.join("live.db-shm")] {
            if p.exists() {
                assert_eq!(mode_of(p), 0o600, "{} must land 0600", p.display());
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Audit finding 17: a dump taken while a multi-table write is in flight
    /// must hold a single point-in-time snapshot, not a mix of per-statement
    /// reads. A writer toggles two tables atomically (the same cycle number
    /// in both); every dump must show the same cycle in both tables.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dump_is_a_single_point_in_time_snapshot() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tmp_dir("dump-snapshot");
        let db = connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.join("snap.db").display()
        ))
        .await
        .unwrap();
        db.init().await.unwrap();
        let backend = db.conn.get_database_backend();
        db.conn
            .execute_raw(Statement::from_string(
                backend,
                "INSERT INTO app_settings (key, value) VALUES ('dump_marker_a', 'v0')",
            ))
            .await
            .unwrap();
        db.conn
            .execute_raw(Statement::from_string(
                backend,
                "INSERT INTO installed_apps (id, data) VALUES ('dump_marker', '{\"n\": 0}')",
            ))
            .await
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_writer = stop.clone();
        let db_writer = db.clone();
        let writer = tokio::spawn(async move {
            let backend = db_writer.conn.get_database_backend();
            let mut n = 0u64;
            while !stop_writer.load(Ordering::Relaxed) {
                n += 1;
                let txn = db_writer.conn.begin().await.unwrap();
                txn.execute_raw(Statement::from_string(
                    backend,
                    &format!(
                        "UPDATE app_settings SET value = 'v{n}' WHERE key = 'dump_marker_a'"
                    ),
                ))
                .await
                .unwrap();
                txn.execute_raw(Statement::from_string(
                    backend,
                    &format!(
                        "INSERT OR REPLACE INTO installed_apps (id, data) VALUES ('dump_marker', '{{\"n\": {n}}}') "
                    ),
                ))
                .await
                .unwrap();
                txn.commit().await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            n
        });

        for i in 0..30 {
            let path = dir.join(format!("dump_{i}.json"));
            db.dump(path.to_str().unwrap()).await.unwrap();
            let data: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            // The app_settings marker holds the cycle as "v<n>".
            let app_cycle = data["tables"]["app_settings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["key"] == "dump_marker_a")
                .and_then(|r| r["value"].as_str())
                .unwrap()
                .trim_start_matches('v')
                .parse::<u64>()
                .unwrap();
            // The installed_apps marker holds the same cycle in its JSON
            // `data` column; extract_cell re-parses JSON-looking strings, so
            // accept both a parsed object and a raw string.
            let marker = data["tables"]["installed_apps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == "dump_marker")
                .cloned()
                .expect("installed_apps marker row missing from dump");
            let marker_cycle = marker["data"]["n"].as_u64().unwrap_or_else(|| {
                serde_json::from_str::<serde_json::Value>(marker["data"].as_str().unwrap())
                    .unwrap()["n"]
                    .as_u64()
                    .unwrap()
            });
            assert_eq!(
                app_cycle, marker_cycle,
                "dump {i} straddles a writer cycle (app_settings v{app_cycle} vs installed_apps n{marker_cycle})"
            );
        }
        stop.store(true, Ordering::Relaxed);
        writer.await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    async fn fresh_db_at(dir: &std::path::Path) -> Db {
        let db = connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.join("vreckan.db").display()
        ))
        .await
        .unwrap();
        db.init().await.unwrap();
        db
    }

    async fn session_count(db: &Db) -> usize {
        entities::session_record::Entity::find().all(&db.conn).await.unwrap().len()
    }

    async fn session_share_count(db: &Db) -> usize {
        entities::session_share::Entity::find().all(&db.conn).await.unwrap().len()
    }

    /// Write a dump file, rewrite the chosen table entry to the given JSON
    /// value (`null` removes the entry), and return the path.
    async fn dump_with_table(db: &Db, dir: &std::path::Path, table: &str, entry: serde_json::Value) -> String {
        let path = dir.join(format!("{table}.dump.json")).to_string_lossy().into_owned();
        db.dump(&path).await.unwrap();
        let mut data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let tables = data.get_mut("tables").unwrap().as_object_mut().unwrap();
        match entry {
            serde_json::Value::Null => {
                tables.remove(table);
            }
            other => {
                tables.insert(table.to_string(), other);
            }
        }
        std::fs::write(&path, data.to_string()).unwrap();
        path
    }

    /// F3: a dump that is missing a table's entry must abort the restore
    /// BEFORE any DELETE runs — the live table keeps its rows (an older
    /// build's or a hand-trimmed archive must not silently empty a table it
    /// never carried).
    #[tokio::test]
    async fn restore_aborts_when_dump_entry_is_missing() {
        let dir = tmp_dir("restore-missing");
        let db = fresh_db_at(&dir).await;
        entities::session_record::Entity::insert(entities::session_record::ActiveModel {
            session_id: sea_orm::ActiveValue::Set("keep-me".to_string()),
            data: sea_orm::ActiveValue::Set(serde_json::json!({ "v": 1 })),
        })
        .exec(&db.conn)
        .await
        .unwrap();
        assert_eq!(session_count(&db).await, 1);

        let path = dump_with_table(&db, &dir, "sessions", serde_json::Value::Null).await;
        let err = db.restore(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("sessions"),
            "error must name the incomplete table: {err}"
        );
        assert_eq!(
            session_count(&db).await,
            1,
            "aborted restore must not touch the live table"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F3: same, for the audit's "trimmed archive where the entry is {}"
    /// case — a present-but-non-array entry is incomplete too.
    #[tokio::test]
    async fn restore_aborts_when_dump_entry_is_not_an_array() {
        let dir = tmp_dir("restore-not-array");
        let db = fresh_db_at(&dir).await;
        entities::session_share::Entity::insert(entities::session_share::ActiveModel {
            share_id: sea_orm::ActiveValue::Set("keep-me".to_string()),
            data: sea_orm::ActiveValue::Set(serde_json::json!({ "v": 1 })),
        })
        .exec(&db.conn)
        .await
        .unwrap();
        assert_eq!(session_share_count(&db).await, 1);

        let path = dump_with_table(&db, &dir, "session_shares", serde_json::json!({})).await;
        let err = db.restore(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("session_shares"),
            "error must name the incomplete table: {err}"
        );
        assert_eq!(
            session_share_count(&db).await,
            1,
            "aborted restore must not touch the live table"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F3: the one exemption — new-style dumps carry no `auth_tokens` entry,
    /// and restore must leave the live token table untouched while
    /// restoring everything else.
    #[tokio::test]
    async fn restore_leaves_auth_tokens_alone_when_entry_is_missing() {
        let dir = tmp_dir("restore-auth-tokens");
        let db = fresh_db_at(&dir).await;
        entities::auth_token::Entity::insert(entities::auth_token::ActiveModel {
            token: sea_orm::ActiveValue::Set("live-token".to_string()),
            username: sea_orm::ActiveValue::Set("admin".to_string()),
            expires_at: sea_orm::ActiveValue::Set(
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64())
                    + 3600.0,
            ),
        })
        .exec(&db.conn)
        .await
        .unwrap();
        assert_eq!(
            entities::auth_token::Entity::find().all(&db.conn).await.unwrap().len(),
            1
        );

        // A dump excludes auth_tokens entirely (new-style dump semantics).
        let path = dump_with_table(&db, &dir, "sessions", serde_json::Value::Array(vec![])).await;
        let tables: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            tables["tables"].get("auth_tokens").is_none(),
            "sanity: dumps must exclude auth_tokens"
        );
        db.restore(&path).await.unwrap();
        assert_eq!(
            entities::auth_token::Entity::find().all(&db.conn).await.unwrap().len(),
            1,
            "restore must not touch the live token table"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F3 sanity: a complete dump still restores end-to-end — the sessions
    /// table is replaced by the dump's rows (rows added after the dump are
    /// gone, dump rows are back).
    #[tokio::test]
    async fn restore_round_trip_replaces_table_contents() {
        let dir = tmp_dir("restore-round-trip");
        let db = fresh_db_at(&dir).await;
        for id in ["s1", "s2"] {
            entities::session_record::Entity::insert(entities::session_record::ActiveModel {
                session_id: sea_orm::ActiveValue::Set(id.to_string()),
                data: sea_orm::ActiveValue::Set(serde_json::json!({ "id": id })),
            })
            .exec(&db.conn)
            .await
            .unwrap();
        }
        let path = dir.join("full.dump.json").to_string_lossy().into_owned();
        db.dump(&path).await.unwrap();

        // The live table gains a third row after the dump was taken.
        entities::session_record::Entity::insert(entities::session_record::ActiveModel {
            session_id: sea_orm::ActiveValue::Set("s3".to_string()),
            data: sea_orm::ActiveValue::Set(serde_json::json!({ "id": "s3" })),
        })
        .exec(&db.conn)
        .await
        .unwrap();
        assert_eq!(session_count(&db).await, 3);

        db.restore(&path).await.unwrap();
        let rows = entities::session_record::Entity::find().all(&db.conn).await.unwrap();
        let ids: Vec<_> = rows.iter().map(|r| r.session_id.clone()).collect();
        assert_eq!(ids.len(), 2, "restore must replace, not merge: {ids:?}");
        assert!(!ids.contains(&"s3".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
