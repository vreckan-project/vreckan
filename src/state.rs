//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Process-wide runtime state.
//!
//! Ports the module-level global dicts in `server/api`
//! (`SESSIONS_DB`, `CRYPTO_SESSIONS`, `AUTH_TOKENS`, `OIDC_STATES`,
//! `PULL_PROGRESS`, `DOWNLOAD_TOKENS`, ...). In Python these are module-level
//! dicts guarded by `asyncio.Lock`; here they are fields of a single
//! `AppState` struct held in axum's state, guarded by `tokio::sync::RwLock`.
//!
//! The DB-backed durable stores (users, groups, apps, sessions, tokens,
//! shares, mounts, stores, templates) are loaded into these maps at startup
//! and persisted back on change, exactly as the Python managers do.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, Semaphore};

use axum_server::tls_rustls::RustlsConfig;

use crate::db::Db;
use crate::settings::Settings;

/// A server-issued web-login token (opaque string -> username + expiry).
#[derive(Clone, Debug)]
pub struct AuthTokenRecord {
    pub username: String,
    pub expires_at: f64,
}

/// An in-flight OIDC authorization state, keyed by the opaque `state` string.
#[derive(Clone, Debug)]
pub struct OidcStateRecord {
    pub expires_at: f64,
}

/// A short-lived public-share download token.
#[derive(Clone, Debug)]
pub struct DownloadTokenRecord {
    pub share_id: String,
    pub expires_at: f64,
}

/// Bookkeeping for the background auto-update job (ANT-269): the wall-clock
/// times (unix seconds) of the last completed tick and the next scheduled
/// tick. In-memory only — after a restart both are `None` until the first
/// tick completes.
#[derive(Clone, Debug, Default)]
pub struct AutoUpdateJobState {
    pub last_check_at: Option<f64>,
    pub next_check_at: Option<f64>,
}

/// Login-attempt accounting for one (client IP, username) pair, used by the
/// brute-force limiter on `/api/auth/login`.
#[derive(Clone, Debug, Default)]
pub struct LoginAttemptWindow {
    /// Failed attempts recorded in the current window.
    pub fails: u32,
    /// Wall-clock start (unix seconds) of the current failure window.
    pub window_start: f64,
    /// While nonzero, the pair is locked out until this time (unix seconds).
    pub locked_until: f64,
}

/// Per-session selkies data-plane connections: session id ->
/// [(token presented to selkies, connect sequence)] in connect order
/// (ANT-443).
type SelkiesClientConns = HashMap<String, Vec<(String, u64)>>;

/// The shared application state, passed to every axum handler.
#[derive(Clone)]
pub struct AppState {
    pub settings: Settings,
    /// The configuration database handle.
    pub db: Db,
    /// Live sessions, keyed by session id. The full session dict is stored as
    /// JSON so the proxy and launch code can read/write arbitrary fields.
    pub sessions: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    /// Per-session E2EE AES keys, keyed by session id.
    pub crypto_sessions: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    /// Web-login auth tokens, keyed by the opaque token string.
    pub auth_tokens: Arc<RwLock<HashMap<String, AuthTokenRecord>>>,
    /// Login-attempt windows for the brute-force limiter, keyed by
    /// "ip:username". In-memory only: a server restart clears the counters.
    pub login_attempts: Arc<RwLock<HashMap<String, LoginAttemptWindow>>>,
    /// In-flight OIDC authorization states, keyed by the opaque `state` string.
    pub oidc_states: Arc<RwLock<HashMap<String, OidcStateRecord>>>,
    /// Short-lived public-share download tokens.
    pub download_tokens: Arc<RwLock<HashMap<String, DownloadTokenRecord>>>,
    /// Live image-pull progress, keyed by image name. Uses a `std::sync::Mutex`
    /// (not a tokio lock) because the Docker pull progress callback is a
    /// synchronous closure that must update it without an `.await`.
    pub pull_progress: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    /// Live image-pull status, keyed by image name
    /// ("queued" / "pulling" / "pull_failed"). Mirrors `api.PULL_STATUS`.
    pub pull_status: Arc<RwLock<HashMap<String, String>>>,
    /// In-process cap on concurrent image pulls (ANT-668), seeded from the
    /// `registry_pull_concurrency` setting at startup. The semaphore lives
    /// in a mutex so [`AppState::pull_semaphore`] can swap it when the
    /// effective DB-over-env-over-default value changes (an admin edit then
    /// applies without a restart). In-process on purpose: it caps the
    /// fan-out of this server regardless of where the Docker daemon runs
    /// (remote `DOCKER_HOST` included).
    pub pull_sem: Arc<Mutex<Arc<Semaphore>>>,
    /// Permit count of the cached `pull_sem`; `pull_semaphore()` rebuilds
    /// when the effective setting differs from it.
    pub pull_sem_permits: Arc<std::sync::atomic::AtomicUsize>,
    /// Cached image metadata, keyed by image name. Besides the local `sha`
    /// (image ID) / `digests` / `last_checked_at`, entries may carry the
    /// registry comparison from an update check: `remote_digest`,
    /// `update_available`, and `last_update_checked_at` (ANT-269).
    /// Mirrors `api.IMAGE_METADATA`.
    pub image_metadata: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    /// Bookkeeping for the background auto-update job (ANT-269): last
    /// completed tick + next scheduled tick (unix seconds). In-memory only.
    pub auto_update_job: Arc<RwLock<AutoUpdateJobState>>,
    /// Installed applications, keyed by app id (the application catalog).
    pub installed_apps: Arc<RwLock<HashMap<String, crate::catalog::InstalledApp>>>,
    /// The configured app stores (name + catalog URL).
    pub app_stores: Arc<RwLock<Vec<crate::catalog::AppStore>>>,
    /// Application templates, keyed by template name (env/docker overrides).
    pub app_templates: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    /// The reconciled template schema (base-layer env + docker overrides).
    pub template_schema: Arc<RwLock<serde_json::Value>>,
    /// GPUs detected on the host.
    pub available_gpus: Arc<RwLock<Vec<crate::docker_provider::GpuInfo>>>,
    /// The admin-set global default GPU (a device path), if any.
    pub global_default_gpu: Arc<RwLock<Option<String>>>,
    /// Container-path -> host-path prefix map (for path remapping when the
    /// server itself runs in a container).
    pub path_prefix_map: Arc<RwLock<HashMap<String, String>>>,
    /// The Docker network the server's own container is on (used to attach
    /// app containers so they can reach the proxy).
    pub discovered_network: Arc<RwLock<Option<String>>>,
    /// In-flight async file-deletion tasks, keyed by task id. Mirrors
    /// `api.DELETION_TASKS`.
    pub deletion_tasks: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    /// Public-share metadata, keyed by share id. Mirrors
    /// `api.PUBLIC_SHARES_METADATA`.
    pub public_shares: Arc<RwLock<HashMap<String, crate::files::PublicShareMetadata>>>,
    /// Session-share (share-session link) metadata, keyed by share id
    /// (ANT-305).
    pub session_shares: Arc<RwLock<HashMap<String, crate::session_shares::SessionShareMetadata>>>,
    /// Per-visit session-share tokens, keyed by token (ANT-305). The session
    /// proxy uses this map to admit a visitor whose session was never
    /// promoted to selkies secure mode; WP3 resolves a visitor's
    /// capabilities through it.
    pub session_share_tokens:
        Arc<RwLock<HashMap<String, crate::session_shares::SessionShareTokenRecord>>>,
    /// Live collaboration-room WebSocket connections, keyed by session id.
    /// Mirrors `collaboration.ROOM_CONNECTIONS`.
    pub room_connections: Arc<RwLock<HashMap<String, crate::collaboration::RoomConnections>>>,
    /// Live data-plane WebSocket connections to selkies per session (ANT-443):
    /// session id -> [(token presented to selkies, connect sequence)] in
    /// connect order. The session proxy updates it as bridges come up and go
    /// down; the plain-session token-table refresh grants `mk_control` to the
    /// most recently connected controller (the "newest-controller-wins"
    /// hand-off selkies' primary kill enforces).
    pub selkies_client_conns: Arc<RwLock<SelkiesClientConns>>,
    /// Monotonic connect sequence for `selkies_client_conns` (in-memory only;
    /// ordering within one vreckan process is all the derivation needs).
    pub selkies_conn_seq: Arc<std::sync::atomic::AtomicU64>,
    /// The last token table each container accepted, keyed by session id
    /// (ANT-443): lets the connect/disconnect refresh skip a re-push when the
    /// derived table is unchanged. Each entry is stamped with the generation
    /// of the derivation that produced the push (ANT-470).
    pub last_pushed_token_tables:
        Arc<RwLock<HashMap<String, crate::collaboration::PushedTokenTable>>>,
    /// Per-session token-table refresh generation (ANT-470): a refresh stamps
    /// itself before reading the connection snapshot, so a refresh that sees
    /// a newer snapshot always carries the larger generation; a push from an
    /// older generation is dropped when a newer one has already been pushed.
    pub token_table_generations: Arc<RwLock<HashMap<String, u64>>>,
    /// When true, the background image-pull task is a no-op (it just marks
    /// the image "queued" and returns without talking to Docker). Tests set
    /// this so the `tokio::spawn`-ed pull doesn't do a real (slow/hanging)
    /// bollard pull and block the test runtime from draining. Mirrors the
    /// Python conftest monkeypatch of `_do_pull_and_cache_image` to a no-op.
    /// Shared across `AppState` clones via `Arc`, so a test that flips it on
    /// its own state is seen by the router's clone.
    pub pull_noop: Arc<std::sync::atomic::AtomicBool>,
    /// When true, `launch_common` uses a fake Docker provider (no daemon) and
    /// records the launch kwargs + app config into `fake_provider_record`.
    /// Mirrors the Python test's `FakeProvider` monkeypatch.
    pub fake_provider: Arc<std::sync::atomic::AtomicBool>,
    /// The recorded launch kwargs + app config from the most recent fake
    /// provider launch (see `fake_provider`).
    pub fake_provider_record: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    /// Test hook: the launch-time selkies capability probe result for fake
    /// provider launches (ANT-438). `None` (the default) means the probe
    /// reports the 2.0.0-class happy path (secure).
    pub fake_selkies_probe: Arc<RwLock<Option<crate::catalog::SelkiesProbe>>>,
    /// The TLS listener's `RustlsConfig` (an `Arc<ArcSwap<ServerConfig>>`). The
    /// ACME issuance/renewal paths reload this in place so a renewed certificate
    /// is served by the running process without a restart or a port rebind.
    /// `None` until the binary has bound a TLS listener (e.g. in unit tests that
    /// drive the router directly without serving TLS).
    pub tls_config: Option<RustlsConfig>,
}

impl AppState {
    pub fn new(settings: Settings, db: Db) -> Self {
        let pull_permits = settings.registry_pull_concurrency.clamp(1, 128) as usize;
        Self {
            settings,
            db,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            crypto_sessions: Arc::new(RwLock::new(HashMap::new())),
            auth_tokens: Arc::new(RwLock::new(HashMap::new())),
            login_attempts: Arc::new(RwLock::new(HashMap::new())),
            oidc_states: Arc::new(RwLock::new(HashMap::new())),
            download_tokens: Arc::new(RwLock::new(HashMap::new())),
            pull_progress: Arc::new(Mutex::new(HashMap::new())),
            pull_status: Arc::new(RwLock::new(HashMap::new())),
            pull_sem: Arc::new(Mutex::new(Arc::new(Semaphore::new(pull_permits)))),
            pull_sem_permits: Arc::new(std::sync::atomic::AtomicUsize::new(pull_permits)),
            image_metadata: Arc::new(RwLock::new(HashMap::new())),
            auto_update_job: Arc::new(RwLock::new(AutoUpdateJobState::default())),
            installed_apps: Arc::new(RwLock::new(HashMap::new())),
            app_stores: Arc::new(RwLock::new(Vec::new())),
            app_templates: Arc::new(RwLock::new(HashMap::new())),
            template_schema: Arc::new(RwLock::new(serde_json::json!({ "settings": [] }))),
            available_gpus: Arc::new(RwLock::new(Vec::new())),
            global_default_gpu: Arc::new(RwLock::new(None)),
            path_prefix_map: Arc::new(RwLock::new(HashMap::new())),
            discovered_network: Arc::new(RwLock::new(None)),
            deletion_tasks: Arc::new(RwLock::new(HashMap::new())),
            public_shares: Arc::new(RwLock::new(HashMap::new())),
            session_shares: Arc::new(RwLock::new(HashMap::new())),
            session_share_tokens: Arc::new(RwLock::new(HashMap::new())),
            room_connections: Arc::new(RwLock::new(HashMap::new())),
            selkies_client_conns: Arc::new(RwLock::new(HashMap::new())),
            selkies_conn_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_pushed_token_tables: Arc::new(RwLock::new(HashMap::new())),
            token_table_generations: Arc::new(RwLock::new(HashMap::new())),
            pull_noop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fake_provider: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fake_provider_record: Arc::new(std::sync::Mutex::new(None)),
            fake_selkies_probe: Arc::new(RwLock::new(None)),
            tls_config: None,
        }
    }

    /// The pull semaphore with the *current* effective
    /// `registry_pull_concurrency` (DB-over-env-over-default, clamped to
    /// 1–128). Returns the cached semaphore when the effective value is
    /// unchanged; otherwise rebuilds it under the `pull_sem` mutex. A
    /// permit held on a just-replaced semaphore stays valid, so a
    /// concurrent rebuild can only transiently widen the cap by one, never
    /// deadlock. The mutex section contains no `.await`.
    pub async fn pull_semaphore(&self) -> Arc<Semaphore> {
        let permits = self
            .settings
            .get_setting(&self.db, "registry_pull_concurrency")
            .await
            .as_i64()
            .unwrap_or(2)
            .clamp(1, 128) as usize;
        let mut slot = self
            .pull_sem
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.pull_sem_permits.load(std::sync::atomic::Ordering::Relaxed) != permits {
            *slot = Arc::new(Semaphore::new(permits));
            self.pull_sem_permits.store(permits, std::sync::atomic::Ordering::Relaxed);
        }
        slot.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pull semaphore tracks the effective `registry_pull_concurrency`
    /// (DB-over-env-over-default): it is cached while the value is
    /// unchanged, rebuilt when the DB layer overrides it, and shared across
    /// `AppState` clones (ANT-668).
    #[tokio::test]
    async fn pull_semaphore_tracks_effective_setting() {
        let dir = std::env::temp_dir().join(format!(
            "vreckan-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.join("vreckan.db").display()
        ))
        .await
        .unwrap();
        db.init().await.unwrap();
        let mut settings = Settings::from_env();
        settings.registry_pull_concurrency = 2;
        let state = AppState::new(settings, db.clone());

        // env/default layer: 2 permits, served from the cache.
        let sem = state.pull_semaphore().await;
        assert_eq!(sem.available_permits(), 2);

        // A DB override wins: the semaphore is rebuilt with the new width.
        db.set_app_setting("VRECKAN_REGISTRY_PULL_CONCURRENCY", "5")
            .await
            .unwrap();
        let sem2 = state.pull_semaphore().await;
        assert_eq!(sem2.available_permits(), 5);
        assert!(!Arc::ptr_eq(&sem, &sem2));

        // Unchanged effective value: the rebuilt semaphore is cached.
        let sem3 = state.pull_semaphore().await;
        assert!(Arc::ptr_eq(&sem2, &sem3));

        // Clones share the same live semaphore (the router holds clones).
        let sem4 = state.clone().pull_semaphore().await;
        assert!(Arc::ptr_eq(&sem2, &sem4));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
