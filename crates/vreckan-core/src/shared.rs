//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Shared data types referenced across crate boundaries
//! (ANT-662 Phase 2 / Candidate C, ANT-770).
//!
//! These types moved into `vreckan-core` because [`crate::state::AppState`]
//! stores them (and the two CSP helpers are called from domain-crate
//! handlers): keeping them here is what gives the workspace its acyclic
//! crate graph. All are plain data — the bounded 7-type extraction
//! (ANT-773) re-verifies that property and the no-`core -> apps`/`core ->
//! admin` invariant on the landed tree.
//!
//! Provenance (pre-split location):
//! - `EnvVar`, `AvailableAppProviderConfig`, `InstalledAppProviderConfig`,
//!   `InstalledApp`, `AppStore`, `SelkiesProbe`, `default_true` — `catalog`
//!   (now `vreckan_apps::catalog`)
//! - `GpuInfo` — `docker_provider` (now `vreckan_apps::docker_provider`)
//! - `PublicShareMetadata` — `files` (now `vreckan_apps::files`)
//! - `SessionShareCaps`, `SessionShareMetadata`, `SessionShareTokenRecord` —
//!   `session_shares` (now `vreckan_apps::session_shares`)
//! - `ConnectionInfo`, `RoomConnections`, `PushedTokenTable` —
//!   `collaboration` (now `vreckan_apps::collaboration`)
//! - `SESSION_CSP`, `strict_csp` — `security` (now `vreckan_admin::security`)
//!
//! Note: `SessionShareTokenRecord` and `PushedTokenTable` are held by
//! `AppState` but were not in the original 7-type list; they are part of
//! the compile closure of the move.

use std::collections::HashMap;

use axum::extract::ws::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// From `catalog` (vreckan_apps::catalog)
/// A single environment variable to pass into an app container.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}
/// The provider configuration for an *available* (store-listed) app.
///
/// Mirrors `models.AvailableAppProviderConfig`. The `docker_overrides` field is
/// a free-form dict of Docker run-options (privileged, cap_add, devices, ...)
/// applied when the app is launched.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AvailableAppProviderConfig {
    pub image: String,
    pub port: i64,
    #[serde(default)]
    pub nvidia_support: bool,
    #[serde(default)]
    pub dri3_support: bool,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub url_support: bool,
    #[serde(default)]
    pub open_support: bool,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_autostart_script_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_autostart_wayland_script_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_overrides: Option<Value>,
    /// The Selkies server version the image runs (e.g. `"2.0.0"`), or `None`
    /// for images that predate the embedded Selkies stack (the classic
    /// xrdp/guacd stack has no `selkies` binary and no token API). Read from
    /// the store catalog's `selkiesVersion` when present; absent from older
    /// catalogs, so callers must treat `None` as "unknown — probe at deploy
    /// time" rather than "pre-2.0".
    #[serde(
        default,
        rename = "selkiesVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub selkies_version: Option<String>,
}
/// The provider configuration for an *installed* app: the available-app config
/// plus per-app env overrides and the (optional) CLI env var a URL is passed
/// through. Mirrors `models.InstalledAppProviderConfig`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledAppProviderConfig {
    #[serde(flatten)]
    pub base: AvailableAppProviderConfig,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_env_var: Option<String>,
}
/// An installed application (a row in the `installed_apps` table, stored as
/// JSON). Mirrors `models.InstalledApp`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledApp {
    pub id: String,
    pub name: String,
    pub logo: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub source_app_id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub home_directories: bool,
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    pub provider_config: InstalledAppProviderConfig,
    #[serde(default = "default_true")]
    pub auto_update: bool,
    #[serde(default)]
    pub app_template: String,
    #[serde(default)]
    pub is_meta_app: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_template_name: Option<String>,
}
pub fn default_true() -> bool {
    true
}
/// An app store: a name plus the URL of its catalog (a YAML file of available
/// apps). Mirrors `models.AppStore`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppStore {
    pub name: String,
    pub url: String,
}
/// The outcome of the launch-time selkies capability probe. Plain sessions
/// enter selkies secure mode at launch (master token + owner controller
/// entry in the token table); containers that lack the selkies 2.x token API
/// (pre-2.0 xrdp/guacd stacks) degrade to the documented legacy path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelkiesProbe {
    /// The container runs a selkies 2.x with a working token API.
    Secure { version: Option<String> },
    /// The container predates the selkies token API (or rejected the
    /// initial token table): the session stays on the legacy path.
    Legacy { version: Option<String> },
}
impl SelkiesProbe {
    pub fn is_secure(&self) -> bool {
        matches!(self, SelkiesProbe::Secure { .. })
    }
}
// From `docker_provider` (vreckan_apps::docker_provider)
/// A GPU device detected on the host (mirrors an entry of `api.AVAILABLE_GPUS`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuInfo {
    pub device: String,
    pub driver: String,
    /// "nvidia" or "dri3".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Index for nvidia GPUs (0-based).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
}
// From `files` (vreckan_apps::files)
#[derive(Serialize, Deserialize, Clone)]
pub struct PublicShareMetadata {
    pub owner_username: String,
    pub original_filename: String,
    pub created_at: f64,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_timestamp: Option<f64>,
}
// From `session_shares` (vreckan_apps::session_shares)
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
    /// When the visit was minted (unix seconds).
    pub created_at: f64,
    /// When the grant expires (unix seconds), if it has a TTL: share visits
    /// inherit the share link's expiry, joins carry the join TTL (ANT-467).
    /// `None` means "no expiry".
    pub expiry: Option<f64>,
}
// From `collaboration` (vreckan_apps::collaboration)
/// A single live WebSocket connection in a room.
#[derive(Clone)]
pub struct ConnectionInfo {
    pub username: String,
    pub token: String,
    pub public_id: String,
    pub has_joined: bool,
    /// Channel used to push broadcast messages to this client's WebSocket.
    pub sender: tokio::sync::mpsc::Sender<Message>,
}
/// The live connections for one collaboration room.
#[derive(Default, Clone)]
pub struct RoomConnections {
    pub controller: Option<ConnectionInfo>,
    pub viewers: HashMap<String, ConnectionInfo>,
}
/// The last token table a container accepted for a session (ANT-443),
/// stamped with the generation of the derivation that produced the push
/// (ANT-470).
#[derive(Debug, Clone)]
pub struct PushedTokenTable {
    pub generation: u64,
    pub table: Value,
}
// From `security` (vreckan_admin::security)
/// The looser CSP used for the reverse-proxied session page (noVNC and other
/// third-party app-container HTML that may rely on inline script/style).
/// Mirrors `_SESSION_CSP` in `server/api`.
///
/// `data:` is allowed in `script-src`/`worker-src` because the selkies
/// app-container loads its Playback AudioWorklet from a `data:text/javascript`
/// URL (ANT-537); without it the browser blocks the worklet and in-container
/// audio dies. The page already allows `'unsafe-inline'`, so a `data:` script
/// adds no real capability.
pub const SESSION_CSP: &str = "default-src 'self'; \
     script-src 'self' 'unsafe-inline' blob: data:; \
     worker-src 'self' blob: data:; \
     style-src 'self' 'unsafe-inline'; \
     img-src 'self' data: blob: https://raw.githubusercontent.com https://github.com; \
     frame-src 'self'; \
     connect-src 'self'; \
     frame-ancestors 'self'; \
     base-uri 'self'; \
     form-action 'self'";
/// A strict, per-response CSP with a nonce, for the collaboration room and
/// the public password page.
pub fn strict_csp(nonce: &str) -> String {
    format!(
        "default-src 'self'; \
         script-src 'self' 'nonce-{nonce}' blob:; \
         worker-src 'self' 'nonce-{nonce}' blob:; \
         style-src 'self' 'nonce-{nonce}'; \
         img-src 'self' data: blob:; \
         connect-src 'self' ws: wss:; \
         frame-ancestors 'self'; \
         base-uri 'self'; \
         form-action 'self'"
    )
}
