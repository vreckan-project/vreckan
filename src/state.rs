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
use tokio::sync::RwLock;

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
    /// Cached image metadata (sha + last_checked_at), keyed by image name.
    /// Mirrors `api.IMAGE_METADATA`.
    pub image_metadata: Arc<RwLock<HashMap<String, serde_json::Value>>>,
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
    /// Live collaboration-room WebSocket connections, keyed by session id.
    /// Mirrors `collaboration.ROOM_CONNECTIONS`.
    pub room_connections: Arc<RwLock<HashMap<String, crate::collaboration::RoomConnections>>>,
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
}

impl AppState {
    pub fn new(settings: Settings, db: Db) -> Self {
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
            image_metadata: Arc::new(RwLock::new(HashMap::new())),
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
            room_connections: Arc::new(RwLock::new(HashMap::new())),
            pull_noop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fake_provider: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fake_provider_record: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}
