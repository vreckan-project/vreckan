//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! PostgreSQL-specific configuration-database tests.
//!
//! Ports `tests/test_postgres`. These exercise the code paths that branch
//! on the database dialect: the schema-builder "migrate" (adding missing
//! columns to a pre-existing table), the JSON column mapping, and the
//! backend-agnostic `dump`/`restore` round-trip.
//!
//! Each test gets a dedicated `Db` pointed at a session-scoped Postgres
//! server (mirrors the `postgres_url` fixture). The server is either an
//! externally-provided one (`VRECKAN_TEST_POSTGRES_URL` / a Postgres
//! `VRECKAN_DATABASE_URL`) or a throwaway `postgres:16` container spun up on
//! the local Docker daemon. The schema is dropped and recreated for every
//! test, so each starts from a clean database.
//!
//! These tests are skipped (not failed) when no Postgres server is reachable
//! and no Docker daemon is available, so the suite stays runnable anywhere.

mod common;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bollard::query_parameters::CreateImageOptions;
use futures_util::StreamExt;
use sea_orm::{ConnectionTrait, Statement};
use vreckan::db;
use vreckan::db::Db;
use vreckan::settings::Settings;
use vreckan::state::AppState;
use vreckan::user_manager;

// ---------------------------------------------------------------------------
// Shared Postgres server (session-scoped, like the Python fixture)
// ---------------------------------------------------------------------------

/// A Postgres server backing the tests: a connection URL plus (optionally) a
/// throwaway container that must be torn down.
#[derive(Debug)]
struct PgServer {
    url: String,
    /// `None` when no Docker daemon is reachable (the skip-marker and
    /// env-URL paths never use it; only a spun-up container needs one).
    client: Option<bollard::Docker>,
    /// Empty when the server came from an env var (nothing to clean up).
    container_id: String,
    /// Where this process's container marker file lives (ANT-378).
    marker_dir: std::path::PathBuf,
}

static PG: OnceLock<PgServer> = OnceLock::new();
static PG_LOCK: Mutex<()> = Mutex::new(());

/// Is a Docker daemon reachable? (Checked inside a tokio runtime so the
/// bollard ping has a time driver.)
async fn docker_available() -> bool {
    match bollard::Docker::connect_with_local_defaults() {
        Ok(c) => c.ping().await.is_ok(),
        Err(_) => false,
    }
}

/// Normalise a Python-style asyncpg URL into a sqlx URL.
fn normalize_pg_url(u: &str) -> String {
    u.replace("postgresql+asyncpg://", "postgres://")
        .replace("postgresql://", "postgres://")
}

/// Resolve an externally-provided Postgres URL, if any. Mirrors the first two
/// branches of the `postgres_url` fixture.
fn resolve_pg_url() -> Option<String> {
    if let Ok(u) = std::env::var("VRECKAN_TEST_POSTGRES_URL") {
        if !u.trim().is_empty() {
            return Some(normalize_pg_url(u.trim()));
        }
    }
    if let Ok(u) = std::env::var("VRECKAN_DATABASE_URL") {
        if u.starts_with("postgresql") || u.starts_with("postgres") {
            return Some(normalize_pg_url(&u));
        }
    }
    None
}

/// Resolve a container's IP address from its network settings.
fn container_ip(ci: &bollard::models::ContainerInspectResponse) -> Option<String> {
    let networks = ci.network_settings.as_ref()?.networks.as_ref()?;
    for ep in networks.values() {
        if let Some(ip) = &ep.ip_address {
            if !ip.is_empty() {
                return Some(ip.clone());
            }
        }
    }
    None
}

/// Marker directory for throwaway Postgres containers (ANT-378). Lives in
/// the working tree — not in the container's /tmp — so a marker written by
/// a run that gets SIGKILLed (run teardown, timeout, OOM) survives and the
/// next run's sweep finds the orphan. Falls back to the system temp dir when
/// the CWD is not writable.
fn pg_marker_dir() -> std::path::PathBuf {
    let dir = std::env::current_dir().unwrap_or_default().join(".vreckan-test-pg");
    if std::fs::create_dir_all(&dir).is_ok() {
        return dir;
    }
    let dir = std::env::temp_dir().join("vreckan-test-pg");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Record the spun-up container (and its anonymous volume) in a per-PID
/// marker file so a killed test process can be swept on the next run
/// (ANT-378). The hostname identifies the container that created the marker;
/// a different hostname means the PID namespace is unrelated and the marker
/// is unconditionally stale.
fn write_pg_marker(dir: &std::path::Path, container_id: &str, volumes: &[String]) {
    let marker = dir.join(format!("pg-{}.json", std::process::id()));
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    let body = serde_json::json!({
        "container_id": container_id,
        "volumes": volumes,
        "pid": std::process::id(),
        "hostname": hostname,
        "created_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    if let Err(e) = std::fs::write(&marker, serde_json::to_string(&body).unwrap_or_default()) {
        eprintln!("warning: could not write postgres test marker {marker:?}: {e}");
    }
}

/// Remove this process's marker (graceful teardown; the sweep then finds
/// nothing to do).
fn remove_pg_marker(dir: &std::path::Path) {
    let _ = std::fs::remove_file(dir.join(format!("pg-{}.json", std::process::id())));
}

/// Is `pid` still alive? Uses the Linux /proc tree; conservatively reports
/// "alive" when undetermined so the sweep never reaps a container another
/// live process owns.
fn pid_alive(pid: u32) -> bool {
    std::fs::metadata(format!("/proc/{pid}")).is_ok()
}

/// Sweep stale markers: for each marker whose owner is dead (different
/// hostname, or same hostname with a dead PID), stop + remove the orphaned
/// container and its anonymous volumes, and log the container's exit state
/// so a killed run stops being a mystery in the infra health check (ANT-378).
async fn sweep_stale_pg_markers(client: &bollard::Docker, dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let current_hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("pg-") || !name.ends_with(".json") {
            continue;
        }
        let Some(marker) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        else {
            let _ = std::fs::remove_file(&path); // unparseable: drop it
            continue;
        };
        let Some(pid) = marker.get("pid").and_then(|v| v.as_u64()) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let container_id = marker
            .get("container_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // A marker from a different container (hostname mismatch) is
        // unconditionally stale: PIDs are per-container and meaningless
        // across namespaces.
        let marker_hostname = marker
            .get("hostname")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let is_stale = if !marker_hostname.is_empty() && marker_hostname != current_hostname {
            true
        } else {
            !pid_alive(pid as u32)
        };
        if !is_stale {
            continue; // owner still running in this container
        }
        let volumes = marker
            .get("volumes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !container_id.is_empty() {
            match client.inspect_container(container_id, None).await {
                Ok(ci) => {
                    let status = ci
                        .state
                        .as_ref()
                        .and_then(|s| s.status.as_ref())
                        .map(|st| st.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let exit_code: i64 = ci
                        .state
                        .as_ref()
                        .and_then(|s| s.exit_code)
                        .unwrap_or(-1);
                    let running = ci.state.as_ref().and_then(|s| s.running).unwrap_or(false);
                    eprintln!(
                        "sweeping orphaned postgres test container {} (hostname={marker_hostname}, \
                         pid {pid}; container {status}, exit {exit_code}; created {})",
                        &container_id[..container_id.len().min(12)],
                        marker.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0)
                    );
                    if running {
                        let _ = client
                            .stop_container(
                                container_id,
                                Some(bollard::query_parameters::StopContainerOptions {
                                    signal: None,
                                    t: Some(2),
                                }),
                            )
                            .await;
                    }
                    let _ = client.remove_container(container_id, None).await;
                }
                Err(_) => {
                    // Container already gone; drop its volume + marker below.
                }
            }
        }
        for vol in &volumes {
            let opts: Option<bollard::query_parameters::RemoveVolumeOptions> = None;
            let _ = client.remove_volume(vol, opts).await;
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Spin up a throwaway `postgres:16` container and wait until it accepts
/// connections. Mirrors the container branch of the `postgres_url` fixture.
async fn spin_up_postgres(marker_dir: &std::path::Path) -> (String, bollard::Docker, String) {
    let client = bollard::Docker::connect_with_local_defaults()
        .expect("connect docker for postgres");
    // Pull postgres:16 if the daemon doesn't have it yet (fresh runners);
    // creating a container from a missing image is a hard 404.
    if client.inspect_image("postgres:16").await.is_err() {
        let options = CreateImageOptions {
            from_image: Some("postgres:16".to_string()),
            ..Default::default()
        };
        let mut stream = client.create_image(Some(options), None, None);
        while let Some(event) = stream.next().await {
            event.expect("pull postgres:16");
        }
    }
    // Honor VRECKAN_TEST_POSTGRES_NETWORK (mirrors the Python fixture): the
    // test container sits on the CI's shared network, so the throwaway
    // postgres must join it too or its assigned IP is unreachable.
    let mut host_config = bollard::models::HostConfig::default();
    if let Ok(net) = std::env::var("VRECKAN_TEST_POSTGRES_NETWORK") {
        if !net.trim().is_empty() {
            host_config.network_mode = Some(net.trim().to_string());
        }
    }
    let body = bollard::models::ContainerCreateBody {
        image: Some("postgres:16".to_string()),
        env: Some(vec![
            "POSTGRES_USER=vreckan".to_string(),
            "POSTGRES_PASSWORD=vreckan".to_string(),
            "POSTGRES_DB=vreckan".to_string(),
        ]),
        host_config: Some(host_config),
        ..Default::default()
    };
    let cid = client
        .create_container(None, body)
        .await
        .expect("create postgres container")
        .id;
    client
        .start_container(&cid, None)
        .await
        .expect("start postgres container");

    // Record the container + its anonymous volume in the marker file so a
    // SIGKILLed run (run teardown / timeout / OOM) can be swept by the next
    // one: the drop guard below only fires on a graceful process exit
    // (ANT-378).
    let volume_names: Vec<String> = client
        .inspect_container(&cid, None)
        .await
        .ok()
        .and_then(|ci| ci.mounts)
        .map(|mounts| {
            mounts
                .into_iter()
                .filter(|m| m.typ.as_deref() == Some("volume"))
                .filter_map(|m| m.name)
                .collect()
        })
        .unwrap_or_default();
    write_pg_marker(marker_dir, &cid, &volume_names);

    // Wait for the container to be assigned an IP.
    let mut ip: Option<String> = None;
    for _ in 0..60 {
        if let Ok(ci) = client.inspect_container(&cid, None).await {
            if let Some(i) = container_ip(&ci) {
                ip = Some(i);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let ip = ip.expect("postgres container never got an IP");
    let url = format!("postgres://vreckan:vreckan@{ip}:5432/vreckan");

    // Wait for Postgres to finish initialising and accept connections.
    for _ in 0..120 {
        if db::connect(&url).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    (url, client, cid)
}

/// Get (or lazily create) the shared Postgres server.
// The lock deliberately spans the await points below (double-checked init).
#[allow(clippy::await_holding_lock)]
async fn pg_server() -> &'static PgServer {
    if let Some(s) = PG.get() {
        return s;
    }
    let _g = PG_LOCK.lock().unwrap();
    if let Some(s) = PG.get() {
        return s;
    }
    // Sweep markers left behind by a killed previous run before we create
    // our own container (ANT-378). Runs on every pg_server() call so the
    // next postgres test run after a SIGKILL cleans up, whichever URL path
    // this run takes. No Docker daemon (e.g. a hermetic CI runner) just
    // skips the sweep.
    let marker_dir = pg_marker_dir();
    if let Ok(sweep_client) = bollard::Docker::connect_with_local_defaults() {
        if sweep_client.ping().await.is_ok() {
            sweep_stale_pg_markers(&sweep_client, &marker_dir).await;
        }
    }

    let server = match resolve_pg_url() {
        Some(url) => PgServer {
            url,
            // Tolerate a missing daemon here: an env-provided URL needs no
            // container, and panicking would poison `PG_LOCK` for the other
            // tests in this binary.
            client: bollard::Docker::connect_with_local_defaults().ok(),
            container_id: String::new(),
            marker_dir,
        },
        None => {
            if !docker_available().await {
                eprintln!("skipping postgres tests: no Postgres URL and no Docker daemon");
                // Park a marker with an empty URL; each test checks `pg_ready`
                // before running. The connect just failed, so the client is
                // `None` — tolerate it instead of `.expect()`-ing a second
                // time (that panic is what poisoned `PG_LOCK` and turned the
                // 6 skips into 6 failures on docker-less runners).
                let marker = PgServer {
                    url: String::new(),
                    client: bollard::Docker::connect_with_local_defaults().ok(),
                    container_id: String::new(),
                    marker_dir,
                };
                PG.set(marker).ok();
                return PG.get().unwrap();
            }
            let (url, client, cid) = spin_up_postgres(&marker_dir).await;
            PgServer {
                url,
                client: Some(client),
                container_id: cid,
                marker_dir,
            }
        }
    };
    PG.set(server).ok();
    PG.get().unwrap()
}

/// True when the shared server is usable (a non-empty URL means we resolved
/// or spun one up; an empty URL means we should skip).
fn pg_ready(s: &PgServer) -> bool {
    !s.url.is_empty()
}

/// Best-effort teardown of a spun-up container.
///
/// The `PgServer` is a `'static` (a `OnceLock`), so on a normal test-run exit
/// its `Drop` fires during process teardown — after every `#[tokio::test]`
/// runtime is gone. `Handle::try_current()` then fails and the old guard
/// silently did nothing, leaving the container behind (ANT-378). The guard
/// now falls back to a short-lived runtime, and removes the marker file only
/// when the container was actually removed — otherwise the next run's sweep
/// collects the leftover.
impl Drop for PgServer {
    fn drop(&mut self) {
        if self.container_id.is_empty() {
            return;
        }
        let cid = self.container_id.clone();
        // A non-empty `container_id` only comes from `spin_up_postgres`,
        // which always produced a live client.
        let client = self.client.clone().expect("spun-up server has a client");
        let cid_short = cid.clone();
        let teardown = async move {
            let _ = client
                .stop_container(
                    &cid,
                    Some(bollard::query_parameters::StopContainerOptions {
                        signal: None,
                        t: Some(2),
                    }),
                )
                .await;
            client.remove_container(&cid, None).await
        };
        let outcome = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(teardown),
            Err(_) => match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(teardown),
                Err(e) => {
                    eprintln!("warning: postgres test teardown: no runtime ({e}); \
                              marker left for the next run's sweep");
                    return;
                }
            },
        };
        if let Err(e) = outcome {
            eprintln!(
                "warning: postgres test container {cid_short} teardown failed: {e}; \
                  marker left for the next run's sweep"
            );
            return;
        }
        remove_pg_marker(&self.marker_dir);
    }
}

// ---------------------------------------------------------------------------
// Per-test database setup
// ---------------------------------------------------------------------------

/// A monotonically-increasing counter so each test gets a unique database
/// name (the tests run in parallel threads and must not share tables).
static DB_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Derive a connection URL for a specific database on the shared server by
/// swapping the last path segment of the base URL.
fn db_url_for(s: &PgServer, dbname: &str) -> String {
    if let Some(pos) = s.url.rfind('/') {
        format!("{}{}", &s.url[..pos + 1], dbname)
    } else {
        format!("{}/{}", s.url, dbname)
    }
}

/// Create a brand-new, uniquely-named database on the shared server, connect
/// to it, and initialise the full schema. Each test gets its own database so
/// the parallel test threads never race on shared tables. Mirrors the
/// `pg_engine` fixture (drop_all + create_all) but per-database.
async fn new_pg_db() -> (Db, String) {
    let s = pg_server().await;
    let n = DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dbname = format!("vreckan_t{n}");
    // Connect to the base database and create the new one.
    let base = db::connect(&s.url).await.expect("connect base db");
    let backend = base.conn.get_database_backend();
    base.conn
        .execute_raw(Statement::from_string(backend, format!("CREATE DATABASE {dbname}")))
        .await
        .expect("create database");
    let d = db::connect(&db_url_for(s, &dbname)).await.expect("connect new db");
    d.init().await.expect("init schema");
    (d, dbname)
}

/// Drop a test database (best effort).
async fn drop_pg_db(s: &PgServer, dbname: &str) {
    let base = match db::connect(&s.url).await {
        Ok(b) => b,
        Err(_) => return,
    };
    let backend = base.conn.get_database_backend();
    let _ = base
        .conn
        .execute_raw(Statement::from_string(backend, format!("DROP DATABASE IF EXISTS {dbname}")))
        .await;
}

/// Build a fresh `AppState` against a brand-new Postgres database (mirrors
/// `build_state` but pointed at Postgres).
async fn build_pg_state() -> (AppState, String) {
    let _s = pg_server().await;
    let (d, dbname) = new_pg_db().await;
    let settings = Settings::from_env();
    let state = AppState::new(settings, d);
    user_manager::load_users_and_groups(&state.db, &state.settings)
        .await
        .expect("load users/groups");
    (state, dbname)
}

/// Return `{column_name: data_type}` for `table` via `information_schema`.
/// Mirrors the Python `_cols` helper (which reports the type's class name).
async fn cols(d: &Db, table: &str) -> HashMap<String, String> {
    let backend = d.conn.get_database_backend();
    let rows = d
        .conn
        .query_all_raw(Statement::from_string(
            backend,
            format!(
                "SELECT column_name, data_type FROM information_schema.columns WHERE table_name = '{table}'"
            ),
        ))
        .await
        .expect("query information_schema");
    let mut out = HashMap::new();
    for row in &rows {
        let name: String = row.try_get("", "column_name").expect("column_name");
        let dtype: String = row.try_get("", "data_type").expect("data_type");
        out.insert(name, dtype);
    }
    out
}

/// The set of table names present in the database.
async fn table_names(d: &Db) -> Vec<String> {
    let backend = d.conn.get_database_backend();
    let rows = d
        .conn
        .query_all_raw(Statement::from_string(
            backend,
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
        ))
        .await
        .expect("query information_schema.tables");
    rows.iter()
        .map(|r| r.try_get::<String>("", "table_name").expect("table_name"))
        .collect()
}

/// Skip the current test (mirrors `pytest.skip`).
fn skip(msg: &str) {
    eprintln!("skipping: {msg}");
}

/// True when a Postgres `data_type` is a real JSON type (not the TEXT
/// fallback SQLite uses).
fn is_json_type(dtype: &str) -> bool {
    matches!(dtype, "json" | "jsonb")
}

/// Delete every row from every table (mirrors the conftest `_wipe_db`).
async fn wipe_db(d: &Db) {
    let backend = d.conn.get_database_backend();
    for table in db::TABLE_ORDER {
        let _ = d
            .conn
            .execute_raw(Statement::from_string(backend, format!("DELETE FROM {table}")))
            .await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `init` creates every table; JSON columns are real JSON types on Postgres.
#[tokio::test]
async fn test_pg_schema_and_jsonb_types() {
    let s = pg_server().await;
    if !pg_ready(s) {
        skip("no postgres server");
        return;
    }
    let (d, dbname) = new_pg_db().await;

    let tables = table_names(&d).await;
    for table in db::TABLE_ORDER {
        assert!(tables.contains(&table.to_string()), "missing table {table}");
    }

    // The entities use the generic `Json` type, which compiles to Postgres's
    // `json` type — a real JSON type, not the "TEXT" fallback SQLite uses.
    let user_cols = cols(&d, "users").await;
    for col in ["settings", "sso_groups", "roles", "permissions", "groups"] {
        let dtype = user_cols.get(col).unwrap_or_else(|| panic!("users.{col} missing"));
        assert!(is_json_type(dtype), "users.{col} is {dtype}, expected a JSON type");
    }
    let group_cols = cols(&d, "groups").await;
    for col in ["settings", "roles", "permissions"] {
        let dtype = group_cols.get(col).unwrap_or_else(|| panic!("groups.{col} missing"));
        assert!(is_json_type(dtype), "groups.{col} is {dtype}, expected a JSON type");
    }
    drop_pg_db(s, &dbname).await;
}

/// The schema-builder "migrate" is a no-op when the schema is already current.
#[tokio::test]
async fn test_pg_migrate_idempotent_on_fresh_schema() {
    let s = pg_server().await;
    if !pg_ready(s) {
        skip("no postgres server");
        return;
    }
    let (d, dbname) = new_pg_db().await;
    // `init` is idempotent: running it twice must not raise.
    d.init().await.expect("second init");
    d.init().await.expect("third init");
    let user_cols = cols(&d, "users").await;
    assert!(user_cols.contains_key("is_sso"));
    assert!(user_cols.contains_key("sso_groups"));
    drop_pg_db(s, &dbname).await;
}

/// The dialect-specific ALTER path: a pre-migration schema gains the new
/// columns as JSON types and `is_sso` as BOOLEAN.
#[tokio::test]
async fn test_pg_migrate_adds_columns_to_legacy_schema() {
    let s = pg_server().await;
    if !pg_ready(s) {
        skip("no postgres server");
        return;
    }
    // A brand-new (empty) database, then build a legacy schema in it.
    let (d, dbname) = new_pg_db().await;
    let backend = d.conn.get_database_backend();

    // Build a legacy schema: users/groups as they shipped before the
    // is_sso/sso_groups/RBAC columns existed.
    for table in db::TABLE_ORDER.iter().rev() {
        let _ = d
            .conn
            .execute_raw(Statement::from_string(backend, format!("DROP TABLE IF EXISTS {table}")))
            .await;
    }
    d.conn
        .execute_raw(Statement::from_string(
            backend,
            "CREATE TABLE users (
                username VARCHAR(255) PRIMARY KEY,
                is_admin BOOLEAN,
                settings JSONB,
                password_hash TEXT
            )",
        ))
        .await
        .expect("create legacy users");
    d.conn
        .execute_raw(Statement::from_string(
            backend,
            "CREATE TABLE groups (
                name VARCHAR(255) PRIMARY KEY,
                settings JSONB
            )",
        ))
        .await
        .expect("create legacy groups");

    // Run the migrate (schema-builder sync) twice: it must add the missing
    // columns and be idempotent.
    d.init().await.expect("migrate");
    d.init().await.expect("migrate again");

    let user_cols = cols(&d, "users").await;
    for col in ["sso_groups", "roles", "permissions", "groups"] {
        assert!(user_cols.contains_key(col), "users.{col} not added by migration");
        let dtype = user_cols.get(col).unwrap();
        assert!(is_json_type(dtype), "users.{col} is {dtype}, expected a JSON type");
    }
    assert!(user_cols.contains_key("is_sso"));
    assert_eq!(user_cols.get("is_sso").unwrap(), "boolean");

    let group_cols = cols(&d, "groups").await;
    for col in ["roles", "permissions"] {
        assert!(group_cols.contains_key(col), "groups.{col} not added by migration");
        let dtype = group_cols.get(col).unwrap();
        assert!(is_json_type(dtype), "groups.{col} is {dtype}, expected a JSON type");
    }
    drop_pg_db(s, &dbname).await;
}

/// dict/list values survive a write + read through the JSON columns.
#[tokio::test]
async fn test_pg_jsonb_roundtrip() {
    let s = pg_server().await;
    if !pg_ready(s) {
        skip("no postgres server");
        return;
    }
    let (d, dbname) = new_pg_db().await;

    user_manager::create_user(&d, "bob", Some(&serde_json::json!({ "public_sharing": true, "storage_limit": 512 })))
        .await
        .expect("create bob");
    let fields = user_manager::UserFields {
        is_admin: Some(false),
        settings: Some(serde_json::json!({ "public_sharing": true, "storage_limit": 512 })),
        roles: Some(vec!["user".to_string(), "operator".to_string()]),
        permissions: Some(vec!["admin.apps".to_string()]),
        groups: Some(vec!["devs".to_string(), "ops".to_string()]),
        sso_groups: Some(vec!["vdi-users".to_string()]),
        ..Default::default()
    };
    user_manager::db_save_user_row(&d.conn, "bob", &fields)
        .await
        .expect("save bob row");
    user_manager::write_group_file(
        &d,
        "devs",
        serde_json::json!({ "gpu": true }),
        Some(vec!["user".to_string()]),
        Some(vec!["admin.templates".to_string()]),
    )
    .await
    .expect("write devs group");

    let u = user_manager::get_user(&d, "bob").await.expect("get bob").expect("bob exists");
    assert_eq!(
        u.settings,
        Some(serde_json::json!({ "public_sharing": true, "storage_limit": 512 }))
    );
    assert_eq!(u.roles, vec!["user", "operator"]);
    assert_eq!(u.permissions, vec!["admin.apps"]);
    assert_eq!(u.groups, vec!["devs", "ops"]);
    assert_eq!(u.sso_groups, vec!["vdi-users"]);

    let g = user_manager::get_group(&d, "devs").await.expect("get devs").expect("devs exists");
    assert_eq!(g.settings, serde_json::json!({ "gpu": true }));
    assert_eq!(g.roles, vec!["user"]);
    assert_eq!(g.permissions, vec!["admin.templates"]);
    drop_pg_db(s, &dbname).await;
}

/// `dump` → wipe → `restore` preserves rows across the JSON backend.
#[tokio::test]
async fn test_pg_dump_restore_roundtrip() {
    let s = pg_server().await;
    if !pg_ready(s) {
        skip("no postgres server");
        return;
    }
    let (d, dbname) = new_pg_db().await;

    user_manager::generate_default_admin(&d, &Settings::from_env())
        .await
        .expect("generate admin");
    user_manager::create_user(&d, "carol", Some(&serde_json::json!({ "public_sharing": true })))
        .await
        .expect("create carol");
    user_manager::set_password(&d, "carol", "carolpass1")
        .await
        .expect("set carol password");
    user_manager::write_group_file(&d, "ops", serde_json::json!({ "gpu": false }), None, None)
        .await
        .expect("write ops group");

    let dump_path = std::env::temp_dir().join(format!("vreckan_pg_dump_{}.json", std::process::id()));
    d.dump(dump_path.to_str().unwrap()).await.expect("dump");

    wipe_db(&d).await;
    d.restore(dump_path.to_str().unwrap()).await.expect("restore");
    let _ = std::fs::remove_file(&dump_path);

    let admin = user_manager::get_user(&d, "admin").await.expect("get admin").expect("admin exists");
    let carol = user_manager::get_user(&d, "carol").await.expect("get carol").expect("carol exists");
    assert!(admin.is_admin);
    assert!(!carol.is_admin);
    let hash = carol.password_hash.expect("carol password hash");
    assert!(hash.starts_with("$argon2"), "unexpected hash: {hash}");

    let ops = user_manager::get_group(&d, "ops").await.expect("get ops").expect("ops exists");
    assert_eq!(ops.settings, serde_json::json!({ "gpu": false }));
    drop_pg_db(s, &dbname).await;
}

/// Session persistence (JSON `data` column) round-trips.
#[tokio::test]
async fn test_pg_sessions_roundtrip() {
    let s = pg_server().await;
    if !pg_ready(s) {
        skip("no postgres server");
        return;
    }
    let (state, dbname) = build_pg_state().await;

    state.sessions.write().await.insert(
        "s-pg".to_string(),
        serde_json::json!({ "instance_id": "ctr-pg", "username": "admin" }),
    );
    vreckan::catalog::save_sessions(&state).await;
    state.sessions.write().await.clear();
    vreckan::catalog::load_sessions(&state).await;

    let sessions = state.sessions.read().await.clone();
    assert_eq!(
        sessions,
        HashMap::from([(
            "s-pg".to_string(),
            serde_json::json!({ "instance_id": "ctr-pg", "username": "admin" })
        )])
    );
    drop_pg_db(s, &dbname).await;
}
