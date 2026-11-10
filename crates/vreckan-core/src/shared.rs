//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Shared data types referenced across crate boundaries
//! (ANT-662 Phase 2 / Candidate C, ANT-770).
//!
//! These types moved into `vreckan-core` because [`crate::state::AppState`]
//! stores them (and the two CSP helpers are called from domain-crate
//! handlers): keeping them here is what gives the workspace its acyclic
//! crate graph. All are plain data — structs and enums holding only other
//! types in this module, other core types, or external-crate types (std,
//! tokio, axum, serde, serde_json). The bounded 7-type extraction
//! (ANT-773) verified that property and the no-`core -> apps`/`core ->
//! admin` edge invariant on the landed tree (2026-11-03): the core
//! manifest depends only on external crates, and the
//! [`ant773_invariant`] test below pins the 7 types' presence and `pub`
//! visibility here.
//!
//! Provenance (pre-split location):
//! - `EnvVar`, `AvailableAppProviderConfig`, `InstalledAppProviderConfig`,
//!   `InstalledApp`, `AppStore`, `SelkiesProbe`, `default_true` — `catalog`
//!   (now `vreckan_catalog::catalog`)
//! - `GpuInfo` — `docker_provider` (now `vreckan_catalog::docker_provider`)
//! - `PublicShareMetadata` — `files` (now `vreckan_files::files`)
//! - `SessionShareCaps`, `SessionShareMetadata`, `SessionShareTokenRecord` —
//!   `session_shares` (now `vreckan_sessions::session_shares`)
//! - `ConnectionInfo`, `RoomConnections`, `PushedTokenTable` —
//!   `collaboration` (now `vreckan_collab::collaboration`)
//! - `SESSION_CSP`, `strict_csp` — `security` (now `vreckan_admin::security`)
//!
//! Additional items the move pulled in (the compile closure, beyond the
//! 7 types named in the ANT-773 issue): `EnvVar`,
//! `AvailableAppProviderConfig`, `InstalledAppProviderConfig`,
//! `default_true` (catalog), `SessionShareCaps`,
//! `SessionShareTokenRecord` (session_shares), `ConnectionInfo`,
//! `PushedTokenTable` (collaboration) — each is a field type of one of
//! the 7 and is itself plain data. `SESSION_CSP`/`strict_csp` moved the
//! other way (admin -> core, not part of the 7-type closure) so the apps
//! handlers keep one CSP source without an `apps -> admin` edge.
//!
//! The ANT-812 central-cluster split (Phase 3) added the small cross-domain
//! helpers the former `vreckan-apps` cycles hinged on: `random_token` /
//! `random_uuid` (formerly in `catalog`), which are plain `rand`-based
//! functions with no core->domain edge of their own.

use std::collections::HashMap;

use axum::extract::ws::Message;
use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// From `catalog` (vreckan_catalog::catalog)
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
// From `docker_provider` (vreckan_catalog::docker_provider)
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
// From `files` (vreckan_files::files)
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
// From `session_shares` (vreckan_sessions::session_shares)
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
// From `collaboration` (vreckan_collab::collaboration)
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

// ---------------------------------------------------------------------------
// Random helpers (moved out of `catalog` in the ANT-812 central-cluster
// split: `files`, `session_shares`, `collaboration`, and `backup_manager`
// all generated tokens/uuids through them, which kept the crate DAG cyclic.
// They are dependency-free apart from `rand`/`base64`/`uuid`.
// ---------------------------------------------------------------------------

/// Generate a random URL-safe token of `n` bytes. Mirrors `secrets.token_urlsafe`.
pub fn random_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
}

/// Generate a random UUID v4 string.
pub fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// True when `segment` is safe to splice into a filesystem path: it is
/// non-empty and contains only ASCII letters, digits, `_` and `-` — no `/`,
/// no `.`, no control or other non-ASCII bytes, so it can never traverse out
/// of its parent directory via a `..`/`/` sequence.
///
/// This is the shared path-traversal allowlist (ANT-1061, code-scanning
/// #5/#85/#88 and #83/#86/#87/#89) for every sink built from an
/// `app.id` or an app-store name: the app-icon filename, the app-store cache
/// filename, and the autostart-script cache filename. It mirrors the
/// allowlist already applied to the icon path in `get_app_icon`.
pub fn is_safe_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Read an installed app's icon if `app_id` is a safe path segment and the
/// derived path stays inside `icons_dir`. The guard order mirrors
/// `get_app_icon` — the CodeQL-recognized taint-kill form for
/// `rust/path-injection`: `is_safe_path_segment` runs on the raw id BEFORE the
/// path is derived, then a standalone negation `starts_with` containment check
/// runs on the derived path, and the read happens in the fall-through. Returns
/// `None` if either guard fails or the read errors (a legacy non-conforming id
/// is inert, not fatal).
pub fn read_app_icon(icons_dir: &str, app_id: &str) -> Option<Vec<u8>> {
    if !is_safe_path_segment(app_id) {
        return None;
    }
    let icon_path = format!("{icons_dir}/{app_id}.png");
    if !std::path::Path::new(&icon_path).starts_with(icons_dir) {
        return None;
    }
    std::fs::read(&icon_path).ok()
}

/// Write an installed app's icon under the same `get_app_icon` guard order.
/// Returns `true` only if the file was actually written inside the icon
/// directory.
pub fn write_app_icon(icons_dir: &str, app_id: &str, data: &[u8]) -> bool {
    if !is_safe_path_segment(app_id) {
        return false;
    }
    let icon_path = format!("{icons_dir}/{app_id}.png");
    if !std::path::Path::new(&icon_path).starts_with(icons_dir) {
        return false;
    }
    std::fs::write(&icon_path, data).is_ok()
}

/// Remove an installed app's icon under the same `get_app_icon` guard order
/// (plus an existence check). Returns `true` only if a file inside the icon
/// directory was removed.
pub fn remove_app_icon(icons_dir: &str, app_id: &str) -> bool {
    if !is_safe_path_segment(app_id) {
        return false;
    }
    let icon_path = format!("{icons_dir}/{app_id}.png");
    if !std::path::Path::new(&icon_path).starts_with(icons_dir) {
        return false;
    }
    if !std::path::Path::new(&icon_path).exists() {
        return false;
    }
    std::fs::remove_file(&icon_path).is_ok()
}

/// Pins the bounded 7-type extraction (ANT-773): every type the issue
/// names must be `pub` in `vreckan-core` (not just reachable through a
/// `pub use` re-export somewhere), so the no-`core -> apps`/`core ->
/// admin` edge stays provable from this crate alone. Referencing a type
/// through a re-export would compile even if the definition left core;
/// naming it as `crate::shared::...` / `crate::state::...` does not.
#[cfg(test)]
mod ant773_invariant {
    use super::*;
    use crate::state::AppState;

    #[test]
    fn the_seven_types_are_pub_in_core() {
        // Name resolution against this crate's own paths: if any of the 7
        // is moved back into a domain crate (or renamed), this stops
        // compiling.
        let _ = std::mem::size_of::<InstalledApp>();
        let _ = std::mem::size_of::<AppStore>();
        let _ = std::mem::size_of::<GpuInfo>();
        let _ = std::mem::size_of::<PublicShareMetadata>();
        let _ = std::mem::size_of::<SessionShareMetadata>();
        let _ = std::mem::size_of::<RoomConnections>();
        let _ = std::mem::size_of::<AppState>();

        // `SelkiesProbe` is an enum: pin the variant shape too (the
        // secure/legacy split is part of the public contract).
        assert!(SelkiesProbe::Secure { version: None }.is_secure());
        assert!(!SelkiesProbe::Legacy { version: None }.is_secure());

        // `RoomConnections` is plain data: default-constructible with
        // `pub` fields (no behavior to exercise here).
        let room = RoomConnections::default();
        assert!(room.controller.is_none());
        assert!(room.viewers.is_empty());
    }
}

#[cfg(test)]
mod ant1061_safe_path_segment {
    use super::is_safe_path_segment;

    #[test]
    fn accepts_safe_ids_and_rejects_traversal() {
        // Safe: ASCII alnum plus `_`/`-`. UUIDs (what `create_meta_app`
        // generates) and the `app-1`/`firefox` test ids all land here.
        assert!(is_safe_path_segment("firefox"));
        assert!(is_safe_path_segment("app-1"));
        assert!(is_safe_path_segment("app_1"));
        assert!(is_safe_path_segment("1234ABCD"));
        assert!(is_safe_path_segment("abc-123_DEF"));
        assert!(is_safe_path_segment("123e4567-e89b-12d3-a456-426614174000"));

        // Unsafe: empty, traversal, absolute, embedded dots, spaces,
        // non-ASCII, and control bytes.
        assert!(!is_safe_path_segment(""));
        assert!(!is_safe_path_segment(".."));
        assert!(!is_safe_path_segment("../x"));
        assert!(!is_safe_path_segment("a/b"));
        assert!(!is_safe_path_segment("/abs"));
        assert!(!is_safe_path_segment("a..b"));
        assert!(!is_safe_path_segment("bad..id"));
        assert!(!is_safe_path_segment("a b"));
        assert!(!is_safe_path_segment("café"));
        assert!(!is_safe_path_segment("a\tb"));
        assert!(!is_safe_path_segment("a\nb"));
    }
}

#[cfg(test)]
mod ant1061_icon_helpers {
    use super::{read_app_icon, remove_app_icon, write_app_icon};

    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ant1061-icons-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn conforming_id_round_trips_inside_dir() {
        let icons = fresh_dir("roundtrip");
        let base = icons.to_string_lossy().to_string();
        assert!(write_app_icon(&base, "firefox", b"PNGDATA"));
        assert_eq!(read_app_icon(&base, "firefox"), Some(b"PNGDATA".to_vec()));
        assert!(remove_app_icon(&base, "firefox"));
        assert_eq!(read_app_icon(&base, "firefox"), None);
        let _ = std::fs::remove_dir_all(&icons);
    }

    #[test]
    fn non_conforming_id_is_inert() {
        let icons = fresh_dir("nonconforming");
        let base = icons.to_string_lossy().to_string();
        // A `../` id is rejected before the path is derived: no write, read,
        // or remove happens (the legacy row is inert, not an escape).
        assert!(!write_app_icon(&base, "../escape", b"X"));
        assert_eq!(read_app_icon(&base, "../escape"), None);
        assert!(!remove_app_icon(&base, "../escape"));
        let _ = std::fs::remove_dir_all(&icons);
    }

    #[test]
    fn traversal_id_cannot_reach_a_sibling_file() {
        let icons = fresh_dir("escaping");
        let base = icons.to_string_lossy().to_string();
        // A secret file the traversal could name from outside the icon dir.
        let secret = icons.parent().unwrap().join("escape.png");
        std::fs::write(&secret, b"secret-bytes").unwrap();
        assert_eq!(read_app_icon(&base, "../escape"), None);
        assert!(!remove_app_icon(&base, "../escape"));
        assert!(secret.exists(), "sibling file must be untouched");
        let _ = std::fs::remove_dir_all(&icons);
        let _ = std::fs::remove_file(&secret);
    }
}
