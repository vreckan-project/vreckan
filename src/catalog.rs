//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! The application catalog: installed-app models, load/save, GPU detection,
//! per-app access control, and the launch + session routes.
//!
//! Ports the catalog/launch portions of `server/api` (`INSTALLED_APPS`,
//! `APP_STORES`, `APP_TEMPLATES`, `TEMPLATE_SCHEMA`, `AVAILABLE_GPUS`,
//! `_launch_common`, `_stop_session`, `_user_can_access_app`, ...) and the thin
//! HTTP endpoints in `server/routers/encrypted` and `server/routers/session`.
//! The Docker-API layer lives in `docker_provider.rs`.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use bollard::Docker;
use rand::Rng;
use sea_orm::{ActiveValue, EntityTrait};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::auth;
use crate::docker_provider::{DockerProvider, GpuInfo, LaunchResult, VolumeSpec};
use crate::e2ee;
use crate::entities;
use crate::session_shares;
use crate::settings::Settings;
use crate::state::AppState;
use crate::user_manager;

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

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

fn default_true() -> bool {
    true
}

/// An app store: a name plus the URL of its catalog (a YAML file of available
/// apps). Mirrors `models.AppStore`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppStore {
    pub name: String,
    pub url: String,
}

/// The public (E2EE) view of an app, returned by `POST /api/applications`.
/// Mirrors `models.Application`. `type` is the store-config app type
/// (`app` | `browser` | `desktop`); data only, no behavioral branching.
#[derive(Clone, Debug, Serialize)]
pub struct Application {
    pub id: String,
    pub name: String,
    pub logo: String,
    pub r#type: String,
    pub home_directories: bool,
    pub nvidia_support: bool,
    pub dri3_support: bool,
    pub url_support: bool,
    pub extensions: Vec<String>,
    pub is_meta_app: bool,
    #[serde(
        default,
        rename = "selkiesVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub selkies_version: Option<String>,
}

/// The response to a launch request. Mirrors `models.LaunchResponse`.
#[derive(Clone, Debug, Serialize)]
pub struct LaunchResponse {
    pub session_url: String,
    pub session_id: String,
}

/// A readiness-probe result for a running session. Mirrors
/// `models.SessionStatusResponse`.
#[derive(Clone, Debug, Serialize)]
pub struct SessionStatusResponse {
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A session as listed for the current user. Mirrors `models.ActiveSessionInfo`.
///
/// Since ANT-440 the list is no longer restricted to the caller's own
/// sessions: every running session is returned so a user can Join one.
/// `session_url` is populated only for the caller's own sessions (it carries
/// their own token); `null` for others', who enter through a Join-minted
/// participant token instead.
#[derive(Clone, Debug, Serialize)]
pub struct ActiveSessionInfo {
    pub session_id: String,
    pub app_id: String,
    pub app_name: String,
    pub app_logo: String,
    pub created_at: f64,
    pub session_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_context: Option<Value>,
    pub is_collaboration: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub out_of_date: bool,
    /// The session's owner (ANT-440): surfaced on other users' sessions so
    /// the list can show whose session it is.
    pub username: String,
    /// True when this session belongs to the caller.
    pub is_mine: bool,
    /// True when the caller can Join this session: the owner's express
    /// grant (ANT-472) — a non-revoked, non-expired named share link that
    /// names the caller, or a persisted participant entry. The caller's
    /// own sessions keep the app-access check; room sessions are never
    /// joinable from this list (their own invite flow).
    pub joinable: bool,
    /// The usernames currently holding a join grant on this session
    /// (ANT-467): set for the caller's own sessions so the list UI can
    /// badge who is in without an extra round trip; empty otherwise
    /// (other users' participants are not the caller's concern).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<String>,
}

/// The response to a session recreate. Mirrors `models.SessionRecreateResponse`.
#[derive(Clone, Debug, Serialize)]
pub struct SessionRecreateResponse {
    pub session_id: String,
    pub session_url: String,
}

/// A GPU device detected on the host (re-exported for the admin status route).
pub type GpuInfoPublic = GpuInfo;

// ---------------------------------------------------------------------------
// Load / save (installed apps + app stores)
// ---------------------------------------------------------------------------

/// Load installed applications from the database into `state.installed_apps`.
/// Mirrors `load_app_configs` (the installed-apps half).
pub async fn load_installed_apps(state: &AppState) {
    let rows = match entities::installed_app::Entity::find().all(&state.db.conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Error loading installed apps from the database: {e}");
            return;
        }
    };
    let mut apps = HashMap::new();
    for row in rows {
        match serde_json::from_value::<InstalledApp>(row.data) {
            Ok(app) => {
                apps.insert(app.id.clone(), app);
            }
            Err(e) => warn!("Skipping malformed installed app row '{}': {e}", row.id),
        }
    }
    let count = apps.len();
    *state.installed_apps.write().await = apps;
    info!("Loaded {count} installed application(s) from the database.");
}

/// Sync the in-memory installed-app set to the database (full replace).
/// Mirrors `save_installed_apps`.
pub async fn save_installed_apps(state: &AppState) {
    let apps = state.installed_apps.read().await.clone();
    let conn = &state.db.conn;
    // Delete all existing rows, then re-insert the current set.
    let all = match entities::installed_app::Entity::find().all(conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to save installed apps (delete): {e}");
            return;
        }
    };
    for row in all {
        let am = entities::installed_app::ActiveModel {
            id: ActiveValue::Set(row.id),
            data: ActiveValue::NotSet,
        };
        if let Err(e) = entities::installed_app::Entity::delete(am).exec(conn).await {
            warn!("Failed to delete installed app row: {e}");
        }
    }
    for app in apps.values() {
        let data = match serde_json::to_value(app) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to serialize installed app: {e}");
                continue;
            }
        };
        let am = entities::installed_app::ActiveModel {
            id: ActiveValue::Set(app.id.clone()),
            data: ActiveValue::Set(data),
        };
        if let Err(e) = entities::installed_app::Entity::insert(am).exec(conn).await {
            warn!("Failed to insert installed app row: {e}");
        }
    }
    info!("Saved {} installed application(s) to the database.", apps.len());
}

/// Load the configured app stores from the database into `state.app_stores`.
/// On first run (no rows) the default SealSkin Apps store is seeded; on the
/// first run after the v0.8.6 rename, the legacy "Vreckan Apps" row is
/// migrated to "SealSkin Apps" (ANT-492). Mirrors `load_app_configs` (the
/// app-stores half).
pub async fn load_app_stores(state: &AppState) {
    let rows = match entities::app_store::Entity::find().all(&state.db.conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Error loading app stores from the database: {e}");
            return;
        }
    };
    if rows.is_empty() {
        let default = AppStore {
            name: "SealSkin Apps".to_string(),
            url: state.settings.app_resource_path.clone(),
        };
        let am = entities::app_store::ActiveModel {
            name: ActiveValue::Set(default.name.clone()),
            url: ActiveValue::Set(default.url.clone()),
        };
        if let Err(e) = entities::app_store::Entity::insert(am).exec(&state.db.conn).await {
            warn!("Failed to seed default app store: {e}");
        }
        *state.app_stores.write().await = vec![default];
        info!("Seeded the default SealSkin Apps store.");
        return;
    }
    let mut rows = rows;
    migrate_vreckan_apps_to_sealskin_apps(state, &mut rows).await;
    let stores: Vec<AppStore> = rows
        .into_iter()
        .map(|r| AppStore { name: r.name, url: r.url })
        .collect();
    let count = stores.len();
    *state.app_stores.write().await = stores;
    info!("Loaded {count} app store(s) from the database.");
}

/// v0.8.6 startup migration (ANT-492): rename the seeded "Vreckan Apps"
/// store to "SealSkin Apps", carrying the rename into the cached catalog
/// file and the installed apps recorded against the store.
///
/// Idempotent: it runs only while a "Vreckan Apps" row exists and no
/// "SealSkin Apps" row does, so a second (or later) start is a no-op. Store
/// names are unique per deployment (`POST /api/admin/apps/stores` rejects
/// duplicates), so a "Vreckan Apps" row can only ever be the seeded default
/// — even on hosts with a custom `VRECKAN_APP_RESOURCE_PATH`.
///
/// `rows` is the store listing just read from the database; it is updated
/// in place so the caller loads the post-migration state.
async fn migrate_vreckan_apps_to_sealskin_apps(
    state: &AppState,
    rows: &mut [entities::app_store::Model],
) {
    const OLD: &str = "Vreckan Apps";
    const NEW: &str = "SealSkin Apps";
    let has_old = rows.iter().any(|r| r.name == OLD);
    let has_new = rows.iter().any(|r| r.name == NEW);
    if !has_old || has_new {
        return;
    }
    let conn = &state.db.conn;
    let old_url = match rows.iter().find(|r| r.name == OLD) {
        Some(r) => r.url.clone(),
        None => return,
    };
    // 1) Rename the store row (insert-new + delete-old, the same shape as
    //    `save_app_stores`'s full-replace; the name is the primary key).
    let am = entities::app_store::ActiveModel {
        name: ActiveValue::Set(NEW.to_string()),
        url: ActiveValue::Set(old_url),
    };
    if let Err(e) = entities::app_store::Entity::insert(am).exec(conn).await {
        warn!("App store migration: failed to insert renamed store: {e}");
        return;
    }
    let del = entities::app_store::ActiveModel {
        name: ActiveValue::Set(OLD.to_string()),
        url: ActiveValue::NotSet,
    };
    if let Err(e) = entities::app_store::Entity::delete(del).exec(conn).await {
        warn!("App store migration: failed to delete old store row: {e}");
        return;
    }
    // 2) Rename the cached catalog file so the renamed store is served from
    //    its existing cache instead of being refetched from scratch.
    let cache_dir = &state.settings.app_store_cache_path;
    let old_file = format!("{cache_dir}/{OLD}.yml");
    let new_file = format!("{cache_dir}/{NEW}.yml");
    if std::path::Path::new(&old_file).exists() {
        if std::path::Path::new(&new_file).exists() {
            // Both files exist: the new-name file wins; drop the stale one.
            let _ = std::fs::remove_file(&old_file);
        } else if let Err(e) = std::fs::rename(&old_file, &new_file) {
            warn!("App store migration: failed to rename cache file: {e}");
        }
    }
    // 3) Backfill `installed_app.source` so the installed-apps table keeps
    //    pointing at the renamed store.
    let mut backfilled = 0;
    if let Ok(app_rows) = entities::installed_app::Entity::find().all(conn).await {
        for row in app_rows {
            if row.data.get("source").and_then(|v| v.as_str()) != Some(OLD) {
                continue;
            }
            let mut data = row.data;
            data["source"] = serde_json::Value::String(NEW.to_string());
            let am = entities::installed_app::ActiveModel {
                id: ActiveValue::Set(row.id),
                data: ActiveValue::Set(data),
            };
            if entities::installed_app::Entity::update(am).exec(conn).await.is_ok() {
                backfilled += 1;
            } else {
                warn!("App store migration: failed to backfill installed app source");
            }
        }
    }
    // 4) Drop the orphaned `app_settings` override row for the session port:
    //    the setting is gone in v0.8.6, so a value left behind would ride
    //    along in every backup dump as dead data (the delete is a no-op when
    //    the row is absent).
    let orphan = entities::app_setting::ActiveModel {
        key: ActiveValue::Set("VRECKAN_SESSION_PORT".to_string()),
        value: ActiveValue::NotSet,
    };
    let _ = entities::app_setting::Entity::delete(orphan).exec(conn).await;
    // Keep the in-memory installed-app set consistent regardless of load
    // order (main.rs loads the installed apps before the stores; the test
    // harness loads the stores first).
    {
        let mut apps = state.installed_apps.write().await;
        for app in apps.values_mut() {
            if app.source == OLD {
                app.source = NEW.to_string();
            }
        }
    }
    // Reflect the rename in the caller's in-memory listing.
    if let Some(pos) = rows.iter().position(|r| r.name == OLD) {
        rows[pos].name = NEW.to_string();
    }
    info!(
        "Migrated the default app store '{OLD}' to '{NEW}' (v0.8.6, ANT-492); backfilled the source of {backfilled} installed app(s)."
    );
}

/// Sync the in-memory app-store list to the database (full replace). Mirrors
/// `save_app_stores`.
pub async fn save_app_stores(state: &AppState) {
    let stores = state.app_stores.read().await.clone();
    let conn = &state.db.conn;
    let all = match entities::app_store::Entity::find().all(conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to save app stores (delete): {e}");
            return;
        }
    };
    for row in all {
        let am = entities::app_store::ActiveModel {
            name: ActiveValue::Set(row.name),
            url: ActiveValue::NotSet,
        };
        if let Err(e) = entities::app_store::Entity::delete(am).exec(conn).await {
            warn!("Failed to delete app store row: {e}");
        }
    }
    for store in stores.iter() {
        let am = entities::app_store::ActiveModel {
            name: ActiveValue::Set(store.name.clone()),
            url: ActiveValue::Set(store.url.clone()),
        };
        if let Err(e) = entities::app_store::Entity::insert(am).exec(conn).await {
            warn!("Failed to insert app store row: {e}");
        }
    }
    info!("Saved {} app store(s) to the database.", stores.len());
}

// ---------------------------------------------------------------------------
// App templates + template schema
// ---------------------------------------------------------------------------

/// Parse a YAML file into a `serde_json::Value` (None on error).
fn yaml_file_to_json(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let yaml_val: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    serde_json::to_value(yaml_val).ok()
}

/// Tag a template value with its provenance (`"default"` for built-in /
/// seeded templates, `"custom"` for user-created ones). The marker is exposed
/// by `GET /api/admin/apps/templates` so clients can hide the template
/// dropdown when no custom templates exist (ANT-655).
pub fn template_with_provenance(mut data: Value, provenance: &str) -> Value {
    if let Some(obj) = data.as_object_mut() {
        obj.insert("provenance".to_string(), Value::String(provenance.to_string()));
    }
    data
}

/// Load application templates into `state.app_templates`.
///
/// Built-in templates ship as YAML files under `default_app_templates_path`;
/// user-defined templates live in the database and override built-ins by name.
/// If no templates exist, a blank "Default" template is seeded. Every entry
/// is tagged with its provenance (`"default"` vs `"custom"`): a DB row that
/// overrides a built-in of the same name is `custom`, except for the seeded
/// blank "Default" row, which stays `default`. Mirrors
/// `load_app_templates`.
pub async fn load_app_templates(state: &AppState) {
    let mut templates: HashMap<String, Value> = HashMap::new();

    // Built-in templates from disk.
    let dir = &state.settings.default_app_templates_path;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_yaml = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "yml" || e == "yaml")
                .unwrap_or(false);
            if !is_yaml {
                continue;
            }
            if let Some(data) = yaml_file_to_json(path.to_str().unwrap_or("")) {
                if let Some(name) = data.get("name").and_then(|v| v.as_str()).map(String::from) {
                    templates.insert(name, template_with_provenance(data, "default"));
                }
            }
        }
    }

    // User-defined templates from the database (override built-ins by name).
    if let Ok(rows) = entities::app_template::Entity::find().all(&state.db.conn).await {
        for row in rows {
            let settings = row.settings.clone();
            // A DB row overriding a built-in of the same name is a custom
            // override. The lone exception is the blank "Default" row seeded
            // on a fresh install (name "Default", empty settings): it must
            // keep reading as `default` on later startups.
            let overrides_builtin = templates.contains_key(&row.name);
            let is_seeded_default = !overrides_builtin
                && row.name == "Default"
                && matches!(&settings, Value::Object(o) if o.is_empty());
            let value = serde_json::json!({ "name": row.name, "settings": settings });
            templates.insert(
                row.name,
                template_with_provenance(
                    value,
                    if is_seeded_default { "default" } else { "custom" },
                ),
            );
        }
    }

    // Seed a blank "Default" template if none exist.
    if templates.is_empty() {
        let default = serde_json::json!({ "name": "Default", "settings": {} });
        let am = entities::app_template::ActiveModel {
            name: ActiveValue::Set("Default".to_string()),
            settings: ActiveValue::Set(serde_json::json!({})),
        };
        if let Err(e) = entities::app_template::Entity::insert(am).exec(&state.db.conn).await {
            warn!("Failed to seed default template: {e}");
        }
        templates.insert("Default".to_string(), template_with_provenance(default, "default"));
    }

    let count = templates.len();
    *state.app_templates.write().await = templates;
    info!("Loaded {count} application template(s).");
}

/// Load the bundled template-schema YAML baseline (the image baseline).
pub fn load_template_schema_yaml(settings: &Settings) -> Vec<Value> {
    let path = &settings.template_schema_path;
    match yaml_file_to_json(path) {
        Some(v) => v
            .get("settings")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default(),
        None => {
            warn!("Could not load template schema ({path}); editor will be limited.");
            Vec::new()
        }
    }
}

/// Load the template schema (and the global default GPU) from the database
/// into `state.template_schema` / `state.global_default_gpu`. If the table is
/// empty it is seeded from the bundled YAML baseline. Mirrors
/// `_init_template_schema` + `_load_template_schema`.
pub async fn load_template_schema(state: &AppState) {
    let conn = &state.db.conn;
    let yaml_settings = load_template_schema_yaml(&state.settings);
    let yaml_names: std::collections::HashSet<String> = yaml_settings
        .iter()
        .filter_map(|s| s.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();

    let mut rows = match entities::template_schema_setting::Entity::find().all(conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to load template schema: {e}");
            return;
        }
    };

    // Seed from the YAML baseline if the table is empty.
    if rows.is_empty() {
        for (i, ys) in yaml_settings.iter().enumerate() {
            let name = match ys.get("name").and_then(|v| v.as_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let am = entities::template_schema_setting::ActiveModel {
                name: ActiveValue::Set(name.clone()),
                sort_order: ActiveValue::Set(i as i32),
                label: ActiveValue::Set(
                    ys.get("label").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                ),
                description: ActiveValue::Set(
                    ys.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                ),
                category: ActiveValue::Set(
                    ys.get("category").and_then(|v| v.as_str()).unwrap_or("general").to_string(),
                ),
                type_: ActiveValue::Set(
                    ys.get("type").and_then(|v| v.as_str()).unwrap_or("text").to_string(),
                ),
                default: ActiveValue::Set(
                    ys.get("default").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                ),
                current: ActiveValue::Set(None),
                docker: ActiveValue::Set(
                    ys.get("docker").and_then(|v| v.as_bool()).unwrap_or(false),
                ),
                options: ActiveValue::Set(
                    ys.get("options").cloned().unwrap_or(serde_json::json!([])),
                ),
            };
            if let Err(e) = entities::template_schema_setting::Entity::insert(am).exec(conn).await {
                warn!("Failed to seed template schema row '{name}': {e}");
            }
        }
        rows = match entities::template_schema_setting::Entity::find().all(conn).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!("Failed to reload template schema: {e}");
                return;
            }
        };
    }

    // Build the in-memory schema.
    let mut settings_list = Vec::new();
    for r in rows {
        let mut entry = serde_json::json!({
            "name": r.name,
            "label": r.label,
            "description": r.description,
            "category": r.category,
            "type": r.type_,
            "default": r.default,
            "current": r.current,
            "docker": r.docker,
            "builtin": yaml_names.contains(&r.name),
        });
        if let Some(a) = r.options.as_array() {
            if !a.is_empty() {
                entry["options"] = r.options;
            }
        }
        settings_list.push(entry);
    }
    *state.template_schema.write().await = serde_json::json!({ "settings": settings_list });

    // Global default GPU (a non-VRECKAN_ app_setting key).
    let gpu = state
        .db
        .get_app_setting("global_default_gpu")
        .await
        .ok()
        .flatten();
    *state.global_default_gpu.write().await = gpu.filter(|g| !g.is_empty());
    info!(
        "Template schema loaded ({} settings); global default GPU: {}.",
        settings_list.len(),
        state.global_default_gpu.read().await.as_deref().unwrap_or("none")
    );
}

// ---------------------------------------------------------------------------
// GPU detection + system stats + self-container inspection
// ---------------------------------------------------------------------------

/// Detect GPUs on the host by reading each render node's driver from sysfs.
/// Mirrors `detect_gpus`. The driver is the basename of the
/// `device/driver` symlink target (e.g. `.../drivers/drm/i915` -> `i915`).
pub async fn detect_gpus(state: &AppState) {
    let mut gpus: Vec<GpuInfo> = Vec::new();
    let drm = "/sys/class/drm";
    if let Ok(entries) = std::fs::read_dir(drm) {
        // Collect renderD* nodes, sorted numerically.
        let mut render_devices: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("renderD"))
            .collect();
        render_devices.sort_by_key(|n| n.trim_start_matches("renderD").parse::<i64>().unwrap_or(0));

        let mut nvidia_index = 0;
        for device in render_devices {
            let driver_path = format!("{drm}/{device}/device/driver");
            let target = std::fs::read_link(&driver_path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let driver = std::path::Path::new(&target)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("")
                .to_string();
            if driver.is_empty() {
                continue;
            }
            let device_path = format!("/dev/dri/{device}");
            let mut gpu = GpuInfo {
                device: device_path,
                driver: driver.clone(),
                r#type: None,
                index: None,
            };
            if driver == "nvidia" {
                gpu.r#type = Some("nvidia".to_string());
                gpu.index = Some(nvidia_index);
                nvidia_index += 1;
            } else {
                gpu.r#type = Some("dri3".to_string());
            }
            gpus.push(gpu);
        }
        info!("Detected {} GPU(s).", gpus.len());
    } else {
        info!("GPU detection could not run ({drm} missing). No GPUs available.");
    }
    *state.available_gpus.write().await = gpus;
}

/// Read the CPU model string from `/proc/cpuinfo` (best-effort).
fn get_cpu_model() -> Option<String> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in cpuinfo.lines() {
        if let Some(rest) = line.strip_prefix("model name") {
            if let Some(value) = rest.split_once(':') {
                return Some(value.1.trim().to_string());
            }
        }
    }
    None
}

/// Gather system stats (CPU model + disk usage of the storage path), cached
/// for 60 seconds. Mirrors `_get_system_stats`.
pub fn get_system_stats(settings: &Settings) -> Value {
    // A full disk-usage query would need a platform call; report the storage
    // path's apparent size as a lightweight proxy for now.
    let size = std::fs::metadata(&settings.storage_path)
        .map(|m| m.len())
        .unwrap_or(0);
    serde_json::json!({
        "cpu_model": get_cpu_model(),
        "disk_total": size,
        "disk_used": size,
    })
}

/// Inspect the server's own container (if it runs in one) to build the
/// container-path -> host-path prefix map and discover the Docker network and
/// external port mappings. Mirrors `_inspect_self_container`.
pub async fn inspect_self_container(state: &AppState) {
    if !std::path::Path::new("/var/run/docker.sock").exists() {
        info!("Docker socket not found. Assuming running on host.");
        return;
    }
    let client = match Docker::connect_with_local_defaults() {
        Ok(c) => c,
        Err(e) => {
            warn!("Could not connect to Docker for self-inspection: {e}");
            return;
        }
    };
    // Find our own container by exact name "vreckan", else by hostname. The
    // daemon's name filter is substring-based, so any other container whose
    // name merely contains "vreckan" (e.g. "vreckan-site" on CI hosts) must
    // not be picked up — only an exact match counts, and only then; the
    // hostname fallback below finds the real self-container otherwise.
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut container = None;
    if let Ok(list) = client
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            filters: Some(HashMap::from([(
                "name".to_string(),
                vec!["vreckan".to_string()],
            )])),
            ..Default::default()
        }))
        .await
    {
        if let Some(c) = list
            .iter()
            .find(|c| c.names.as_ref().is_some_and(|names| {
                names.iter().any(|n| n.trim_start_matches('/') == "vreckan")
            }))
        {
            if let Some(id) = &c.id {
                container = client.inspect_container(id, None).await.ok();
            }
        }
    }
    if container.is_none() && !hostname.is_empty() {
        container = client.inspect_container(&hostname, None).await.ok();
    }
    let container = match container {
        Some(c) => c,
        None => {
            warn!("Could not find self-container. Path remapping disabled.");
            return;
        }
    };
    info!("Found self-container '{}'. Inspecting mounts.", container.name.unwrap_or_default());

    // Build the path prefix map from the container's mounts.
    let mut prefix_map = HashMap::new();
    if let Some(mounts) = &container.mounts {
        for m in mounts {
            if let (Some(src), Some(dst)) = (&m.source, &m.destination) {
                prefix_map.insert(dst.clone(), src.clone());
            }
        }
    }
    if !prefix_map.is_empty() {
        info!("Detected container mount prefixes: {prefix_map:?}");
    }
    *state.path_prefix_map.write().await = prefix_map;

    // Discover the network the container is attached to.
    if let Some(networks) = container
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
    {
        if let Some(first) = networks.keys().next() {
            *state.discovered_network.write().await = Some(first.clone());
            info!("Discovered self-container network: {first}");
        }
    }
}

// ---------------------------------------------------------------------------
// Per-app access control
// ---------------------------------------------------------------------------

/// Return true if the user may use the app.
///
/// The user must be listed in `app.users` or belong to *any* of the groups in
/// `app.groups`. `"*"` / `"all"` are wildcards meaning "every user" / "every
/// group". There is no admin bypass: admins are subject to the same per-app
/// access rules as regular users. `user_groups` is the user's full multi-group
/// membership list. Mirrors `_user_can_access_app`.
pub fn user_can_access_app(app: &InstalledApp, username: &str, user_groups: &[String]) -> bool {
    let allowed_users = &app.users;
    let allowed_groups = &app.groups;
    allowed_users.iter().any(|u| u == "*" || u == "all" || u == username)
        || allowed_groups.iter().any(|g| {
            g == "*" || g == "all" || user_groups.iter().any(|ug| ug == g)
        })
}

// ---------------------------------------------------------------------------
// Launch request models
// ---------------------------------------------------------------------------

/// Body for `POST /api/launch/simple`. Mirrors `models.LaunchRequestSimple`.
#[derive(Clone, Debug, Deserialize)]
pub struct LaunchRequestSimple {
    pub application_id: String,
    #[serde(default)]
    pub home_name: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub selected_gpu: Option<String>,
    #[serde(default)]
    pub launch_in_room_mode: bool,
    #[serde(default = "default_true")]
    pub wayland_mode: bool,
    #[serde(default)]
    pub session_name: Option<String>,
}

/// Body for `POST /api/launch/url`. Mirrors `models.LaunchRequestURL`.
#[derive(Clone, Debug, Deserialize)]
pub struct LaunchRequestURL {
    pub url: String,
    pub application_id: String,
    #[serde(default)]
    pub home_name: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub selected_gpu: Option<String>,
    #[serde(default)]
    pub launch_in_room_mode: bool,
    #[serde(default = "default_true")]
    pub wayland_mode: bool,
    #[serde(default)]
    pub session_name: Option<String>,
}

/// Body for `POST /api/launch/file`. Mirrors `models.LaunchRequestFile`.
#[derive(Clone, Debug, Deserialize)]
pub struct LaunchRequestFile {
    pub application_id: String,
    pub filename: String,
    pub upload_id: String,
    pub total_chunks: i64,
    #[serde(default = "default_true")]
    pub open_file_on_launch: bool,
    #[serde(default)]
    pub home_name: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub selected_gpu: Option<String>,
    #[serde(default)]
    pub launch_in_room_mode: bool,
    #[serde(default = "default_true")]
    pub wayland_mode: bool,
    #[serde(default)]
    pub session_name: Option<String>,
}

/// Body for `POST /api/launch/file_path`. Mirrors `models.LaunchRequestFilePath`.
#[derive(Clone, Debug, Deserialize)]
pub struct LaunchRequestFilePath {
    pub application_id: String,
    #[serde(default)]
    pub home_name: Option<String>,
    pub filename: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub selected_gpu: Option<String>,
    #[serde(default)]
    pub launch_in_room_mode: bool,
    #[serde(default = "default_true")]
    pub wayland_mode: bool,
    #[serde(default)]
    pub session_name: Option<String>,
}

/// Body for `POST /api/sessions/{id}/send_file`. Mirrors
/// `models.SendFileToSessionRequest`.
#[derive(Clone, Debug, Deserialize)]
pub struct SendFileToSessionRequest {
    pub filename: String,
    pub upload_id: String,
    pub total_chunks: i64,
}

// ---------------------------------------------------------------------------
// Launch helpers (env / docker-override layering)
// ---------------------------------------------------------------------------

/// The base-layer env for app launches, from the template schema.
///
/// A non-docker setting is pushed through only when its `current` value (the
/// admin's base-layer override) is set and differs from `default` (the image
/// baseline). This is the lowest-priority env layer. Mirrors `_schema_default_env`.
fn schema_default_env(schema: &Value) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(settings) = schema.get("settings").and_then(|s| s.as_array()) {
        for s in settings {
            if s.get("docker").and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }
            let name = match s.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => continue,
            };
            let current = s.get("current");
            let default = s.get("default").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(c) = current.and_then(|v| v.as_str()) {
                if !c.is_empty() && c != default {
                    env.insert(name.to_string(), c.to_string());
                }
            }
        }
    }
    env
}

/// Parse the `DOCKER_*` entries of a settings dict into docker run-options.
/// Mirrors `_extract_docker_overrides`.
pub fn extract_docker_overrides(settings_dict: &Value) -> HashMap<String, Value> {
    let mut overrides = HashMap::new();
    let obj = match settings_dict.as_object() {
        Some(o) => o,
        None => return overrides,
    };
    let parse_list = |val: &str| -> Vec<String> {
        val.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    };
    let parse_kv = |val: &str, sep: char| -> HashMap<String, String> {
        let mut res = HashMap::new();
        for item in val.split(',') {
            if let Some((k, v)) = item.split_once(sep) {
                res.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        res
    };
    for (k, v) in obj {
        if !k.starts_with("DOCKER_") {
            continue;
        }
        let val = v.as_str().unwrap_or("").trim().to_string();
        if val.is_empty() {
            continue;
        }
        match k.as_str() {
            "DOCKER_PRIVILEGED" => {
                overrides.insert("privileged".into(), Value::Bool(val.eq_ignore_ascii_case("true")));
            }
            "DOCKER_CAP_ADD" => {
                overrides.insert("cap_add".into(), Value::from(parse_list(&val)));
            }
            "DOCKER_CAP_DROP" => {
                overrides.insert("cap_drop".into(), Value::from(parse_list(&val)));
            }
            "DOCKER_SECURITY_OPT" => {
                overrides.insert("security_opt".into(), Value::from(parse_list(&val)));
            }
            "DOCKER_DEVICES" => {
                overrides.insert("devices".into(), Value::from(parse_list(&val)));
            }
            "DOCKER_DNS" => {
                overrides.insert("dns".into(), Value::from(parse_list(&val)));
            }
            "DOCKER_SHM_SIZE" => {
                overrides.insert("shm_size".into(), Value::String(val));
            }
            "DOCKER_MEM_LIMIT" => {
                overrides.insert("mem_limit".into(), Value::String(val));
            }
            "DOCKER_CPU_SHARES" => {
                if let Ok(n) = val.parse::<i64>() {
                    overrides.insert("cpu_shares".into(), Value::from(n));
                }
            }
            "DOCKER_NANO_CPUS" => {
                if let Ok(n) = val.parse::<i64>() {
                    overrides.insert("nano_cpus".into(), Value::from(n));
                }
            }
            "DOCKER_NETWORK_MODE" => {
                overrides.insert("network_mode".into(), Value::String(val));
            }
            "DOCKER_IPC_MODE" => {
                overrides.insert("ipc_mode".into(), Value::String(val));
            }
            "DOCKER_PID_MODE" => {
                overrides.insert("pid_mode".into(), Value::String(val));
            }
            "DOCKER_GROUP_ADD" => {
                overrides.insert("group_add".into(), Value::from(parse_list(&val)));
            }
            "DOCKER_EXTRA_HOSTS" => {
                overrides.insert("extra_hosts".into(), Value::from(parse_list(&val)));
            }
            "DOCKER_SYSCTLS" => {
                overrides.insert("sysctls".into(), serde_json::to_value(parse_kv(&val, '=')).unwrap_or_default());
            }
            "DOCKER_TMPFS" => {
                overrides.insert("tmpfs".into(), serde_json::to_value(parse_kv(&val, ':')).unwrap_or_default());
            }
            "DOCKER_BIND_MOUNTS" => {
                overrides.insert("volumes".into(), Value::from(parse_list(&val)));
            }
            "DOCKER_ENV" => {
                overrides.insert("environment".into(), serde_json::to_value(parse_kv(&val, '=')).unwrap_or_default());
            }
            _ => {}
        }
    }
    overrides
}

/// The base-layer docker run-options, from the template schema. Mirrors
/// `_schema_docker_overrides`.
fn schema_docker_overrides(schema: &Value) -> HashMap<String, Value> {
    let mut raw = HashMap::new();
    if let Some(settings) = schema.get("settings").and_then(|s| s.as_array()) {
        for s in settings {
            if !s.get("docker").and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }
            let name = match s.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => continue,
            };
            let current = s.get("current");
            let default = s.get("default").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(c) = current.and_then(|v| v.as_str()) {
                if !c.is_empty() && c != default {
                    raw.insert(name.to_string(), Value::String(c.to_string()));
                }
            }
        }
    }
    extract_docker_overrides(&serde_json::to_value(raw).unwrap_or_default())
}

/// Merge the base-layer docker overrides into an app config as the
/// lowest-priority layer. Values already present in the app config's own
/// docker_overrides win; list-valued keys are unioned; environment dicts are
/// merged key-by-key. Mirrors `_apply_base_docker_overrides`.
fn apply_base_docker_overrides(app_config_dict: &mut Value, schema: &Value) {
    let base = schema_docker_overrides(schema);
    if base.is_empty() {
        return;
    }
    let obj = match app_config_dict.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let pc = obj
        .entry("provider_config")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let pc_obj = match pc.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let overrides = pc_obj
        .entry("docker_overrides")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let overrides_obj = match overrides.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    for (k, v) in base {
        match overrides_obj.get_mut(&k) {
            None => {
                overrides_obj.insert(k, v);
            }
            Some(existing) => {
                if k == "devices" || k == "volumes" {
                    if let (Some(a), Some(b)) = (existing.as_array_mut(), v.as_array()) {
                        for item in b {
                            if !a.contains(item) {
                                a.push(item.clone());
                            }
                        }
                    }
                } else if k == "environment" {
                    if let (Some(a), Some(b)) = (existing.as_object_mut(), v.as_object()) {
                        for (ek, ev) in b {
                            a.entry(ek.clone()).or_insert_with(|| ev.clone());
                        }
                    }
                }
            }
        }
    }
}

/// Translate an in-container path to its host path using the discovered
/// prefix map. Mirrors `_translate_path_to_host`.
/// Resolve a possibly-relative path to an absolute one (relative to the
/// process CWD). Mirrors Python's `os.path.abspath`. Docker bind mounts must
/// use absolute host paths — a relative source is rejected with "includes
/// invalid characters for a local volume name".
fn abspath(p: &str) -> String {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        return p.to_string();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    cwd.join(path).to_string_lossy().into_owned()
}

pub fn translate_path_to_host(map: &HashMap<String, String>, internal_path: &str) -> String {
    if map.is_empty() || internal_path.is_empty() {
        return internal_path.to_string();
    }
    let mut prefixes: Vec<&String> = map.keys().collect();
    prefixes.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for container_prefix in prefixes {
        if internal_path == container_prefix.as_str()
            || internal_path.starts_with(&format!("{container_prefix}/"))
        {
            let host_prefix = &map[container_prefix];
            let relative = internal_path
                .strip_prefix(container_prefix.as_str())
                .unwrap_or("")
                .trim_start_matches('/');
            if relative.is_empty() {
                return host_prefix.clone();
            }
            return format!("{host_prefix}/{relative}");
        }
    }
    internal_path.to_string()
}

/// Add admin-assigned external volume mounts to a container's volumes dict.
/// Mirrors `_apply_external_mounts`. (The volume-mount manager is ported with
/// the admin endpoints; for now this is a no-op placeholder.)
#[allow(unused_variables)]
fn apply_external_mounts(
    volumes: &mut HashMap<String, VolumeSpec>,
    username: &str,
    host_mount_path: Option<&str>,
) {
    // TODO(port admin): resolve mounts by user/group via volume_mount_manager.
}

/// Sanitize a name for use as a filename. Mirrors `_sanitize_for_filename`.
fn sanitize_for_filename(name: &str) -> String {
    if name.is_empty() {
        return "unnamed".to_string();
    }
    let s = name.to_lowercase().trim().to_string();
    let s: String = s
        .split(|c: char| c.is_whitespace() || c == '_')
        .collect::<Vec<_>>()
        .join("-");
    let s: String = s.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').collect();
    s.chars().take(50).collect()
}

/// Return a unique filename in a directory (appending `-1`, `-2`, ...).
/// Mirrors `_get_unique_filename`.
pub fn get_unique_filename(directory: &str, filename: &str) -> String {
    if !std::path::Path::new(&format!("{directory}/{filename}")).exists() {
        return filename.to_string();
    }
    let (name, ext) = match filename.rsplit_once('.') {
        Some((n, e)) => (n, format!(".{e}")),
        None => (filename, String::new()),
    };
    let mut counter = 1;
    loop {
        let new_filename = format!("{name}-{counter}{ext}");
        if !std::path::Path::new(&format!("{directory}/{new_filename}")).exists() {
            return new_filename;
        }
        counter += 1;
    }
}

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

/// The owner's session URL. Carries the session's `access_token` (the
/// first-hit `initial_session_auth` validates it and sets the session cookie;
/// the WebSocket path and existing consumers rely on it) **and** the same
/// value under `token` — the selkies 2.0.0 WebRTC client sources its
/// `client_token` only from the page URL's `?token=` param, so on a
/// TURN-configured dual-mode image the owner needs it there or signaling
/// rejects the owner's HELLO (`missing or invalid token in secure mode`) and
/// input is dropped (ANT-449). A plain session's owner is addressed by the
/// access token in the token table, so the two params carry the same value.
pub fn owner_session_url(session_id: &str, access_token: &str) -> String {
    format!("/api/apps/session/{session_id}/?access_token={access_token}&token={access_token}")
}

/// Persist the in-memory session map to the database (full replace). Mirrors
/// `save_sessions_to_disk`.
pub async fn save_sessions(state: &AppState) {
    let sessions = state.sessions.read().await.clone();
    let conn = &state.db.conn;
    let all = match entities::session_record::Entity::find().all(conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to save sessions (delete): {e}");
            return;
        }
    };
    for row in all {
        let am = entities::session_record::ActiveModel {
            session_id: ActiveValue::Set(row.session_id),
            data: ActiveValue::NotSet,
        };
        if let Err(e) = entities::session_record::Entity::delete(am).exec(conn).await {
            warn!("Failed to delete session row: {e}");
        }
    }
    for (id, data) in sessions.iter() {
        let am = entities::session_record::ActiveModel {
            session_id: ActiveValue::Set(id.clone()),
            data: ActiveValue::Set(data.clone()),
        };
        if let Err(e) = entities::session_record::Entity::insert(am).exec(conn).await {
            warn!("Failed to insert session row: {e}");
        }
    }
}

/// Load sessions from the database into `state.sessions`. Mirrors
/// `load_sessions_from_disk`.
pub async fn load_sessions(state: &AppState) {
    let rows = match entities::session_record::Entity::find().all(&state.db.conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Error loading sessions from the database: {e}");
            return;
        }
    };
    let mut sessions = HashMap::new();
    for row in rows {
        sessions.insert(row.session_id, row.data);
    }
    let count = sessions.len();
    *state.sessions.write().await = sessions;
    info!("Loaded {count} session(s) from the database.");
}

// ---------------------------------------------------------------------------
// Background image pull
// ---------------------------------------------------------------------------

/// Fold one Docker pull progress event into the aggregate per-image progress
/// state. Mirrors `_update_pull_progress`.
fn update_pull_progress(state: &AppState, image_name: &str, line: &Value) {
    let mut progress = state
        .pull_progress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prog = progress
        .entry(image_name.to_string())
        .or_insert(serde_json::json!({ "status": "pulling", "percentage": null, "current": 0, "total": 0 }));
    if let Some(status) = line.get("status").and_then(|v| v.as_str()) {
        prog["status"] = Value::String(status.to_string());
    }
    if let Some(partial) = line.get("partial").and_then(|v| v.as_str()) {
        if !prog.get("_layers").map(|v| v.is_object()).unwrap_or(false) {
            prog["_layers"] = Value::Object(serde_json::Map::new());
        }
        if let Some(layers_obj) = prog.get_mut("_layers").and_then(|v| v.as_object_mut()) {
            let layer = layers_obj
                .entry(partial.to_string())
                .or_insert_with(|| serde_json::json!({ "current": 0, "total": 0 }));
            if let Some(total) = line.get("total").and_then(|v| v.as_i64()) {
                layer["total"] = Value::Number(total.into());
            } else if let Some(current) = line.get("current").and_then(|v| v.as_i64()) {
                layer["current"] = Value::Number(current.into());
            }
        }
    }
    if let Some(layers) = prog.get("_layers").and_then(|v| v.as_object()) {
        let mut total = 0i64;
        let mut current = 0i64;
        for v in layers.values() {
            let t = v.get("total").and_then(|x| x.as_i64()).unwrap_or(0);
            if t > 0 {
                total += t;
                current += v.get("current").and_then(|x| x.as_i64()).unwrap_or(0);
            }
        }
        if total > 0 {
            prog["total"] = Value::Number(total.into());
            prog["current"] = Value::Number(current.into());
            prog["percentage"] = Value::from(
                ((current as f64 / total as f64) * 1000.0).round() / 10.0,
            );
        }
    }
}

/// Run the actual pull + metadata caching for an image. The caller must have
/// claimed the image in `pull_status` first. On success the status is cleared;
/// on failure it is left as "pull_failed". Mirrors `_do_pull_and_cache_image`.
pub async fn do_pull_and_cache_image(
    state: &AppState,
    image_name: &str,
    app: Option<InstalledApp>,
) {
    // Test hook: when `pull_noop` is set, leave the image "queued" and return
    // without talking to Docker (mirrors the Python conftest no-op pull). This
    // keeps the `tokio::spawn`-ed task from doing a real, slow bollard pull
    // that would block the test runtime from draining.
    if state.pull_noop.load(std::sync::atomic::Ordering::Relaxed) {
        state.pull_status.write().await.insert(image_name.to_string(), "queued".to_string());
        let _ = app;
        return;
    }
    state.pull_status.write().await.insert(image_name.to_string(), "pulling".to_string());
    state
        .pull_progress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            image_name.to_string(),
            serde_json::json!({ "status": "pulling", "percentage": null, "current": 0, "total": 0 }),
        );
    let provider = match DockerProvider::new(serde_json::json!({ "provider_config": { "image": image_name } })) {
        Ok(p) => p,
        Err(e) => {
            state
                .pull_status
                .write()
                .await
                .insert(image_name.to_string(), "pull_failed".to_string());
            warn!("Failed to connect to Docker for pull of '{image_name}': {}", e.detail);
            return;
        }
    };
    let state_clone = state.clone();
    let image_clone = image_name.to_string();
    let progress_cb: std::sync::Arc<dyn Fn(Value) + Send + Sync> =
        std::sync::Arc::new(move |line: Value| {
            update_pull_progress(&state_clone, &image_clone, &line);
        });
    let result = provider.pull_image(image_name, Some(progress_cb)).await;
    match result {
        Ok(()) => {
            // Cache the image metadata (sha + last_checked_at).
            let local_info = provider.get_local_image_info(image_name).await;
            let mut metadata = state.image_metadata.write().await;
            let entry = metadata
                .entry(image_name.to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .unwrap();
            if let Some(info) = local_info {
                entry.insert(
                    "sha".to_string(),
                    info.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()).map(Value::from).unwrap_or(Value::Null),
                );
                entry.insert(
                    "digests".to_string(),
                    info.get("digests").cloned().unwrap_or(Value::Array(vec![])),
                );
            } else {
                entry.insert("sha".to_string(), Value::Null);
                entry.insert("digests".to_string(), Value::Array(vec![]));
            }
            entry.insert(
                "last_checked_at".to_string(),
                Value::from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0),
                ),
            );
            drop(metadata);
            state.pull_status.write().await.remove(image_name);
            state
                .pull_progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(image_name);
            info!("Background pull for '{image_name}' completed successfully.");
        }
        Err(e) => {
            state
                .pull_status
                .write()
                .await
                .insert(image_name.to_string(), "pull_failed".to_string());
            let mut progress = state
                .pull_progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(prog) = progress.get_mut(image_name) {
                prog["status"] = Value::String(format!("failed: {}", e.detail));
            }
            warn!("Background pull for '{image_name}' failed: {}", e.detail);
        }
    }
    let _ = app;
}

/// The current wall-clock time in unix seconds (the shape used by the
/// metadata-cache timestamps).
fn now_unix_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The deterministic fake `get_local_image_info` record used by the
/// `fake_provider` test hook (mirrors the fake in the admin check endpoints).
fn fake_local_image_info() -> Value {
    serde_json::json!({
        "id": "sha256:local",
        "short_id": "local123",
        "digests": ["sha256:local"],
    })
}

/// Merge refreshed image metadata into the in-memory cache (keyed by image
/// name), preserving any fields the update does not refresh. With
/// `local_info` (a `get_local_image_info` result) present, writes the local
/// `sha` (image ID) / `digests` and bumps `last_checked_at`; with
/// `remote_check` (`(remote_digest, update_available)`) present, writes the
/// registry comparison and bumps `last_update_checked_at` (ANT-269).
pub async fn persist_image_metadata(
    state: &AppState,
    image_name: &str,
    local_info: Option<&Value>,
    remote_check: Option<(&str, bool)>,
) {
    if local_info.is_none() && remote_check.is_none() {
        return;
    }
    let mut metadata = state.image_metadata.write().await;
    let entry = metadata
        .entry(image_name.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .unwrap();
    if let Some(info) = local_info {
        entry.insert(
            "sha".to_string(),
            info.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .map(Value::from)
                .unwrap_or(Value::Null),
        );
        entry.insert(
            "digests".to_string(),
            info.get("digests").cloned().unwrap_or(Value::Array(vec![])),
        );
        entry.insert("last_checked_at".to_string(), Value::from(now_unix_secs()));
    }
    if let Some((remote_digest, update_available)) = remote_check {
        entry.insert("remote_digest".to_string(), Value::from(remote_digest));
        entry.insert("update_available".to_string(), Value::Bool(update_available));
        entry.insert("last_update_checked_at".to_string(), Value::from(now_unix_secs()));
    }
}

/// Inspect a locally-present image and cache its metadata (sha / digests /
/// last_checked_at) into `state.image_metadata` — the same entry shape
/// `do_pull_and_cache_image` writes. Best-effort: a missing image or an
/// unreachable daemon is logged and the entry stays unknown. The
/// `fake_provider` test hook substitutes a deterministic fake record.
/// Used by the startup warm-up, the launch path (after a launch-time pull),
/// and the background auto-update job (ANT-269).
pub async fn cache_local_image_metadata(state: &AppState, image_name: &str) {
    let local_info = if state
        .fake_provider
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        Some(fake_local_image_info())
    } else {
        let provider = match DockerProvider::new(serde_json::json!({ "provider_config": { "image": image_name } })) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "Local image metadata lookup failed for '{image_name}': {}",
                    e.detail
                );
                return;
            }
        };
        match provider.get_local_image_info(image_name).await {
            Some(info) => Some(info),
            None => {
                debug!("No local image found for '{image_name}'; leaving metadata unknown.");
                return;
            }
        }
    };
    persist_image_metadata(state, image_name, local_info.as_ref(), None).await;
}

/// Warm the in-memory image-metadata cache at startup (the root-cause fix for
/// the "— unknown" Image column, ANT-266/ANT-269): for each installed app's
/// image (deduped by image name), inspect the local image concurrently and
/// cache its sha/digests, so the installed-apps table shows the real image
/// identity immediately after a restart. Best-effort and non-blocking with
/// respect to failures: a missing image or a down Docker daemon is logged and
/// left unknown, and no `pull_status` changes (this is inspection, not
/// pulling). The `fake_provider` test hook is honored.
pub async fn warm_image_metadata(state: &AppState) {
    let apps = state
        .installed_apps
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let mut images: Vec<String> = apps
        .iter()
        .map(|a| a.provider_config.base.image.clone())
        .collect();
    images.sort();
    images.dedup();
    let mut tasks = Vec::new();
    for image in images {
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            cache_local_image_metadata(&state, &image).await;
        }));
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// One auto-update tick (ANT-269): for each installed app's image (deduped by
/// image name, fetched concurrently, best-effort) refresh the metadata cache
/// with the local sha/digests and the registry's latest digest
/// (`update_available` = the remote digest is not among the local digests),
/// and — when `auto_update_apps` is on and at least one app using the image
/// has its `auto_update` flag on and an update is available — claim the image
/// in `pull_status` and pull it through the same background path the manual
/// Update button uses (`do_pull_and_cache_image`). A missing local image or a
/// down daemon is logged and never crashes the job. The `fake_provider` test
/// hook is honored.
pub async fn run_auto_update_tick(state: &AppState) {
    let apps = state
        .installed_apps
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let mut by_image: HashMap<String, Vec<InstalledApp>> = HashMap::new();
    for app in apps {
        by_image
            .entry(app.provider_config.base.image.clone())
            .or_default()
            .push(app);
    }
    let enabled = state
        .settings
        .get_setting(&state.db, "auto_update_apps")
        .await
        .as_bool()
        .unwrap_or(true);
    let mut tasks = Vec::new();
    for (image, image_apps) in by_image {
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            let (local_info, remote_digest) = if state
                .fake_provider
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let remote = if image == "lscr.io/linuxserver/firefox:latest" {
                    "sha256:local".to_string()
                } else {
                    "sha256:remote".to_string()
                };
                (Some(fake_local_image_info()), Some(remote))
            } else {
                let provider = match DockerProvider::new(
                    serde_json::json!({ "provider_config": { "image": image } }),
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(
                            "Auto-update tick: failed to connect to Docker for '{image}': {}",
                            e.detail
                        );
                        return;
                    }
                };
                (
                    provider.get_local_image_info(&image).await,
                    provider.get_remote_image_digest(&image).await,
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
            persist_image_metadata(
                &state,
                &image,
                local_info.as_ref(),
                remote_digest.as_deref().map(|rd| (rd, update_available)),
            )
            .await;
            if enabled && update_available && image_apps.iter().any(|a| a.auto_update) {
                let already = matches!(
                    state.pull_status.read().await.get(&image).map(|s| s.as_str()),
                    Some("queued") | Some("pulling")
                );
                if !already {
                    state
                        .pull_status
                        .write()
                        .await
                        .insert(image.clone(), "queued".to_string());
                    let app = image_apps
                        .iter()
                        .find(|a| a.auto_update)
                        .cloned()
                        .unwrap_or_else(|| image_apps[0].clone());
                    tokio::spawn(async move {
                        do_pull_and_cache_image(&state, &image, Some(app)).await;
                    });
                }
            }
        }));
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// The scheduled update-check job (ANT-269). The first tick runs immediately
/// at startup (the local metadata warm-up plus the registry digest
/// comparison); afterwards each tick runs every `auto_update_interval_seconds`.
/// Both settings are re-read fresh on every tick (DB-over-env-over-default
/// precedence), so admin edits apply without a restart. When
/// `auto_update_apps` is false the tick is a no-op (mirrors how the ACME job
/// no-ops when ACME is disabled); the local warm-up already ran from
/// `main.rs` at startup in either case. Each completed tick records
/// `last_check_at` / `next_check_at` in `state.auto_update_job` for
/// `GET /api/admin/apps/auto_update_status`.
pub async fn background_auto_update_job(state: AppState) {
    loop {
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
            .unwrap_or(3600)
            .max(1) as u64;
        if enabled {
            run_auto_update_tick(&state).await;
        }
        let now = now_unix_secs();
        {
            let mut job = state.auto_update_job.write().await;
            job.last_check_at = Some(now);
            job.next_check_at = Some(now + interval as f64);
        } // write guard dropped here — readers (e.g. the auto_update_status
        // endpoint) no longer block for the whole sleep (ANT-348)
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

// ---------------------------------------------------------------------------
// Launch-time selkies capability probe (ANT-438)
// ---------------------------------------------------------------------------

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

/// Parse `selkies --version` output (`selkies 2.0.0`) into
/// `(major_version, version_string)`.
pub fn parse_selkies_version(output: &str) -> Option<(u32, String)> {
    for tok in output.split_whitespace() {
        let tok = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
        let mut parts = tok.split('.');
        let Some(major) = parts.next() else { continue };
        let Some(major) = major.parse::<u32>().ok() else { continue };
        let rest_ok = parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if rest_ok {
            return Some((major, tok.to_string()));
        }
    }
    None
}

/// Classify the response of a `GET {subfolder}api/sessions` probe
/// (ANT-438, probe step 1).
///
/// - JSON (200 open / 401 secure): the selkies 2.x token API exists ⇒
///   `Secure`.
/// - 401 with a plain-text body (`"Unauthorized"`): selkies' own secure-mode
///   401 (basic auth passed, session token missing) ⇒ `Secure`. An nginx
///   basic-auth failure, by contrast, answers `text/html`.
/// - HTML: nginx answered instead of selkies — either the pre-2.0
///   xrdp/guacd SPA fallback (Phase 0 finding (f)) or a transient startup
///   error page. Left ambiguous so step 2 (exec) arbitrates.
/// - Anything else: ambiguous.
pub fn classify_sessions_probe(status: u16, content_type: Option<&str>) -> Option<SelkiesProbe> {
    let ct = content_type.unwrap_or("");
    if ct.contains("application/json") || (status == 401 && ct.contains("text/plain")) {
        Some(SelkiesProbe::Secure { version: None })
    } else {
        None
    }
}

/// Probe a freshly launched app container's selkies capability (ANT-438).
///
/// 1. Plain HTTP `GET {subfolder}api/sessions` to the container's port: a
///    JSON response means the selkies 2.x token API is present; an HTML
///    response means the pre-2.0 stack (Phase 0 finding (f)).
/// 2. When that is ambiguous, confirm by exec: `selkies --version` —
///    major < 2 or a missing binary means pre-2.0.
/// 3. When exec is inconclusive too, the initial token-table push is the
///    arbiter — the same fallback the lazy share promotion uses (ANT-305).
///
/// The session must already carry `master_token` (and the `ip`/`port` of the
/// launched container) so step 3 can authenticate the push.
pub async fn probe_selkies_capability(
    state: &AppState,
    provider: &DockerProvider,
    session: &Value,
    session_id: &str,
) -> SelkiesProbe {
    let ip = session.get("ip").and_then(|v| v.as_str()).unwrap_or("");
    let port = session.get("port").and_then(|v| v.as_u64()).unwrap_or(0);
    let url = format!("http://{ip}:{port}/api/apps/session/{session_id}/api/sessions");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    let mut req = client.get(&url);
    if let (Some(user), Some(pass)) = (
        session.get("custom_user").and_then(|v| v.as_str()),
        session.get("password").and_then(|v| v.as_str()),
    ) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        req = req.header(reqwest::header::AUTHORIZATION, format!("Basic {b64}"));
    }
    if let Some(verdict) = match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let ct = resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = resp.text().await.unwrap_or_default();
            let preview: String = body.chars().take(80).collect();
            let verdict = classify_sessions_probe(status, Some(&ct));
            info!(
                "[{session_id}] Capability probe step 1 (GET /api/sessions): status={status} content-type={ct} body={preview:?} => {:?}",
                verdict
            );
            verdict
        }
        Err(e) => {
            info!("[{session_id}] Capability probe step 1 inconclusive (transport): {e}");
            None
        }
    } {
        return verdict;
    }

    // Step 2: confirm by exec when the HTTP answer is ambiguous.
    let instance_id = session.get("instance_id").and_then(|v| v.as_str()).unwrap_or("");
    if let Some((code, out)) =
        provider.container_exec(instance_id, &["selkies".to_string(), "--version".to_string()]).await
    {
        let preview: String = out.chars().take(80).collect();
        info!(
            "[{session_id}] Capability probe step 2 (exec selkies --version): code={code} output={preview:?}"
        );
        if let Some((major, version)) = parse_selkies_version(&out) {
            return if major >= 2 {
                SelkiesProbe::Secure { version: Some(version) }
            } else {
                SelkiesProbe::Legacy { version: Some(version) }
            };
        }
        // The exec ran but the binary is missing: pre-2.0 stack.
        if code == 127 || out.contains("not found") || out.contains("No such file") {
            return SelkiesProbe::Legacy { version: None };
        }
        // Ran without a recognizable version: fall through to step 3.
    }

    // Step 3: the token-table push is the arbiter (ANT-305 fallback).
    if crate::collaboration::push_token_table(state, session_id, session).await {
        SelkiesProbe::Secure { version: None }
    } else {
        SelkiesProbe::Legacy { version: None }
    }
}

// ---------------------------------------------------------------------------
// The launch flow
// ---------------------------------------------------------------------------

/// The shared launch logic behind every launch endpoint. Mirrors
/// `api._launch_common`. Builds the env/volume/GPU config, launches the
/// container via the Docker provider, records the session, and returns the
/// session URL + id.
#[allow(clippy::too_many_arguments)]
pub async fn launch_common(
    state: &AppState,
    application_id: &str,
    username: &str,
    effective_settings: &Value,
    home_name: Option<&str>,
    extra_env: HashMap<String, String>,
    language: Option<&str>,
    selected_gpu: Option<&str>,
    file_bytes: Option<Vec<u8>>,
    filename: Option<&str>,
    open_file_on_launch: bool,
    forced_rw_mount: Option<&str>,
    launch_in_room_mode: bool,
    wayland_mode: bool,
    session_name: Option<&str>,
    check_access: bool,
) -> Result<LaunchResponse, (StatusCode, String)> {
    let settings = &state.settings;

    // --- Resolve the app ----------------------------------------------------
    let app = state
        .installed_apps
        .read()
        .await
        .get(application_id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("Application with ID '{application_id}' not found."),
            )
        })?;

    // --- Per-app access control --------------------------------------------
    if check_access {
        let user_groups = user_manager::get_user(&state.db, username)
            .await
            .map(|u| u.map(|u| u.groups).unwrap_or_default())
            .unwrap_or_default();
        if !user_can_access_app(&app, username, &user_groups) {
            info!(
                "[{username}] Launch denied for '{}': users={:?} groups={:?}",
                app.name, app.users, app.groups
            );
            return Err((
                StatusCode::FORBIDDEN,
                format!("You do not have access to '{}'.", app.name),
            ));
        }
    }

    // --- Session identity ---------------------------------------------------
    let session_id = uuid::Uuid::new_v4().to_string();
    let access_token = random_token(32);
    let subfolder = format!("/api/apps/session/{session_id}/");
    let mut launch_context: Option<Value> = None;

    // Room-mode launches mint the master token + controller/viewer tokens.
    // Plain launches mint the master token too, so the session can enter
    // selkies secure mode at launch with the owner as a first-class
    // controller (ANT-438). A pre-2.0 container ignores the env var and the
    // launch-time capability probe rolls the session back to the legacy
    // path (the documented pre-2.0 fallback).
    let (master_token, controller_token, viewer_token) = if launch_in_room_mode {
        (
            Some(random_token(32)),
            Some(random_token(16)),
            Some(random_token(16)),
        )
    } else {
        (Some(random_token(32)), None, None)
    };
    let custom_user = random_uuid();
    let password = random_uuid();

    // --- Environment (layered) ---------------------------------------------
    let schema = state.template_schema.read().await.clone();
    let mut final_env = schema_default_env(&schema);
    final_env.insert("SUBFOLDER".into(), subfolder);
    final_env.insert("PUID".into(), settings.puid.to_string());
    final_env.insert("PGID".into(), settings.pgid.to_string());
    final_env.insert("CUSTOM_USER".into(), custom_user.clone());
    final_env.insert("PASSWORD".into(), password.clone());
    final_env.insert("TZ".into(), "America/Phoenix".into());
    final_env.insert("SELKIES_ALLOWED_ORIGINS".into(), "*".into());
    if let Some(mt) = &master_token {
        final_env.insert("SELKIES_MASTER_TOKEN".into(), mt.clone());
    }

    // App template env + docker overrides.
    let template = state
        .app_templates
        .read()
        .await
        .get(&app.app_template)
        .cloned();
    let mut docker_overrides_from_template = HashMap::new();
    match &template {
        Some(t) => {
            if let Some(raw) = t.get("settings") {
                let mut template_settings = HashMap::new();
                if let Some(obj) = raw.as_object() {
                    for (k, v) in obj {
                        if !k.starts_with("DOCKER_") {
                            if let Some(s) = v.as_str() {
                                template_settings.insert(k.clone(), s.to_string());
                            }
                        }
                    }
                }
                docker_overrides_from_template = extract_docker_overrides(raw);
                final_env.extend(template_settings);
            }
        }
        None => {
            warn!(
                "[{session_id}] Template '{}' not found for app '{}'. Using container defaults.",
                app.app_template, app.name
            );
        }
    }

    final_env.extend(extra_env);
    if let Some(lang) = language {
        if lang.to_lowercase() != "en_us.utf-8" {
            final_env.insert("LC_ALL".into(), lang.to_string());
        }
    }
    for env_override in &app.provider_config.env {
        final_env.insert(env_override.name.clone(), env_override.value.clone());
    }
    // Always emit an explicit PIXELFLUX_WAYLAND value. Images (e.g. the
    // linuxserver ones) bake `PIXELFLUX_WAYLAND=true` into their config, and a
    // missing launch-env entry would let that default survive a Wayland-off
    // launch. Placed after every other env layer so the launch-time choice is
    // authoritative (on -> "true", off -> "false").
    final_env.insert(
        "PIXELFLUX_WAYLAND".into(),
        if wayland_mode { "true" } else { "false" }.into(),
    );
    if let Some(sn) = session_name {
        final_env.insert("TITLE".into(), format!("{} - {}", app.name, sn));
    }

    // URL launch context + CLI env var mapping.
    if let Some(url) = final_env.get("VRECKAN_URL") {
        launch_context = Some(serde_json::json!({ "type": "url", "value": url }));
        if let Some(url_env_var) = &app.provider_config.url_env_var {
            final_env.insert(url_env_var.clone(), url.clone());
        }
    }

    // --- Docker config (layered overrides) ----------------------------------
    let mut app_config_dict = serde_json::to_value(&app).unwrap_or_default();
    apply_base_docker_overrides(&mut app_config_dict, &schema);
    if !docker_overrides_from_template.is_empty() {
        if let Some(obj) = app_config_dict.as_object_mut() {
            let pc = obj
                .entry("provider_config")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(pc_obj) = pc.as_object_mut() {
                let overrides = pc_obj
                    .entry("docker_overrides")
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(ov_obj) = overrides.as_object_mut() {
                    for (k, v) in docker_overrides_from_template {
                        match ov_obj.get_mut(&k) {
                            None => {
                                ov_obj.insert(k, v);
                            }
                            Some(existing) => {
                                if k == "devices" || k == "volumes" {
                                    if let (Some(a), Some(b)) = (existing.as_array_mut(), v.as_array()) {
                                        for item in b {
                                            if !a.contains(item) {
                                                a.push(item.clone());
                                            }
                                        }
                                    }
                                } else if k == "environment" {
                                    if let (Some(a), Some(b)) = (existing.as_object_mut(), v.as_object()) {
                                        for (ek, ev) in b {
                                            a.entry(ek.clone()).or_insert_with(|| ev.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // --- Volumes + home directory ------------------------------------------
    let mut volumes: HashMap<String, VolumeSpec> = HashMap::new();
    // Every branch of the chain below assigns `host_mount_path`, so no
    // initializer (an initial `None` would be a never-read assignment).
    let mut host_mount_path: Option<String>;
    let mut shared_files_path: Option<String> = None;

    let is_persistent_launch = effective_settings
        .get("persistent_storage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && (home_name.is_none() || home_name.map(|h| h.to_lowercase() != "cleanroom").unwrap_or(true));

    let ephemeral_base = format!("{}/vreckan_ephemeral", settings.storage_path);

    if let Some(forced) = forced_rw_mount {
        let p = forced.to_string();
        std::fs::create_dir_all(&p).ok();
        host_mount_path = Some(p);
    } else if app.is_meta_app {
        let template_path = format!("{}/{}", settings.home_templates_path, app.home_template_name.clone().unwrap_or_default());
        if !std::path::Path::new(&template_path).is_dir() {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "Home directory template for meta app '{}' not found on server.",
                    app.name
                ),
            ));
        }
        if is_persistent_launch {
            let sanitized = sanitize_for_filename(&app.name);
            let dir = format!("auto-{sanitized}");
            let p = format!("{}/{}/{}", settings.storage_path, username, dir);
            if !std::path::Path::new(&p).exists() {
                info!(
                    "[{session_id}] First launch for meta-app. Copying template '{}' for user '{username}'.",
                    app.home_template_name.as_deref().unwrap_or("")
                );
                copy_tree(&template_path, &p);
            }
            host_mount_path = Some(p);
            shared_files_path = Some(format!("{}/{}/_vreckan_shared_files", settings.storage_path, username));
        } else {
            let p = format!("{}/{}", ephemeral_base, uuid::Uuid::new_v4());
            info!(
                "[{session_id}] Launching meta-app in cleanroom mode. Copying template to ephemeral storage."
            );
            copy_tree(&template_path, &p);
            host_mount_path = Some(p);
            shared_files_path = Some(format!("{}/{}_shared", ephemeral_base, session_id));
            std::fs::create_dir_all(shared_files_path.as_ref().unwrap()).ok();
        }
    } else {
        let use_persistent = effective_settings
            .get("persistent_storage")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && app.home_directories;
        let effective_home = if use_persistent { home_name } else { Some("cleanroom") };
        match effective_home {
            Some(h) if h.to_lowercase() != "cleanroom" => {
                let p = std::path::Path::new(&format!("{}/{}/{}", settings.storage_path, username, h))
                    .to_string_lossy()
                    .into_owned();
                if !std::path::Path::new(&p).is_dir() {
                    return Err((
                        StatusCode::NOT_FOUND,
                        format!("Home directory '{h}' not found."),
                    ));
                }
                host_mount_path = Some(p);
                shared_files_path = Some(format!("{}/{}/_vreckan_shared_files", settings.storage_path, username));
            }
            _ => {
                let p = format!("{}/{}", ephemeral_base, uuid::Uuid::new_v4());
                // The ephemeral home dir is bind-mounted into the container, so
                // it must exist on the host before the container is created
                // (Docker rejects a bind whose source path is missing).
                std::fs::create_dir_all(&p).ok();
                let shared = format!("{}/{}_shared", ephemeral_base, session_id);
                std::fs::create_dir_all(&shared).ok();
                host_mount_path = Some(p);
                shared_files_path = Some(shared);
            }
        }
    }

    // --- GPU selection ------------------------------------------------------
    let gpus = state.available_gpus.read().await.clone();
    let global_default = state.global_default_gpu.read().await.clone();
    let mut gpu_config: Option<GpuInfo> = None;
    let user_selected_gpu = selected_gpu.map(|g| !g.is_empty()).unwrap_or(false)
        && effective_settings.get("gpu").and_then(|v| v.as_bool()).unwrap_or(false);
    if user_selected_gpu {
        let sel = selected_gpu.unwrap_or("");
        let gpu_info = gpus.iter().find(|g| g.device == sel).cloned();
        let gpu_info = match gpu_info {
            Some(g) => g,
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("Selected GPU '{sel}' is not available."),
                ))
            }
        };
        if gpu_info.r#type.as_deref() == Some("nvidia") && !app.provider_config.base.nvidia_support {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("App '{}' does not support Nvidia GPUs.", app.name),
            ));
        }
        if gpu_info.r#type.as_deref() == Some("dri3") && !app.provider_config.base.dri3_support {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("App '{}' does not support DRI3 GPUs.", app.name),
            ));
        }
        gpu_config = Some(gpu_info);
    } else if let Some(gd) = &global_default {
        if let Some(gpu_info) = gpus.iter().find(|g| &g.device == gd).cloned() {
            let ok = (gpu_info.r#type.as_deref() != Some("nvidia") || app.provider_config.base.nvidia_support)
                && (gpu_info.r#type.as_deref() != Some("dri3") || app.provider_config.base.dri3_support);
            if ok {
                info!(
                    "[{session_id}] Using global default GPU '{}' for '{}'.",
                    gpu_info.device, app.name
                );
                gpu_config = Some(gpu_info);
            }
        }
    }
    if let Some(g) = &gpu_config {
        if g.r#type.as_deref() == Some("dri3") || (g.r#type.as_deref() == Some("nvidia") && wayland_mode) {
            final_env.insert("DRI_NODE".into(), g.device.clone());
            final_env.insert("DRINODE".into(), g.device.clone());
        }
    }

    // --- Autostart script injection ----------------------------------------
    let (autostart_content, autostart_subpath) = resolve_autostart(state, &app, wayland_mode, &session_id);
    if let (Some(content), Some(subpath)) = (&autostart_content, &autostart_subpath) {
        let mut hmp = host_mount_path.clone();
        if hmp.is_none() {
            let p = format!("{}/{}", ephemeral_base, uuid::Uuid::new_v4());
            info!("[{session_id}] Created ephemeral storage for autostart script.");
            hmp = Some(p);
            host_mount_path = hmp.clone();
        }
        if let Some(hmp) = &hmp {
            let autostart_file = format!("{hmp}/{subpath}");
            if let Some(parent) = std::path::Path::new(&autostart_file).parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if let Err(e) = std::fs::write(&autostart_file, content) {
                warn!("[{session_id}] Failed to write autostart script: {e}");
            }
        }
    }

    // Ephemeral (cleanroom) launch: the home directory lives under the
    // ephemeral base, so the container is short-lived by design. It gets the
    // `no` restart policy (never resurrected by a daemon restart) and is
    // reaped on session stop and on server shutdown (ANT-378).
    let is_ephemeral_launch = host_mount_path
        .as_deref()
        .is_some_and(|p| p.starts_with(&ephemeral_base));

    // --- Build the volume mounts -------------------------------------------
    let prefix_map = state.path_prefix_map.read().await.clone();
    if let Some(hmp) = &host_mount_path {
        // The host (bind source) path must be absolute for Docker; resolve any
        // relative path against the CWD (mirrors Python's `os.path.abspath`).
        let translated = abspath(&translate_path_to_host(&prefix_map, hmp));
        volumes.insert(
            translated,
            VolumeSpec {
                bind: settings.container_config_path.clone(),
                mode: "rw".into(),
            },
        );
    }
    if let Some(sfp) = &shared_files_path {
        std::fs::create_dir_all(sfp).ok();
        let translated = abspath(&translate_path_to_host(&prefix_map, sfp));
        volumes.insert(
            translated,
            VolumeSpec {
                bind: format!("{}/Desktop/files", settings.container_config_path),
                mode: "rw".into(),
            },
        );
        // Write the uploaded file into the shared-files area, optionally open it.
        if let (Some(bytes), Some(fname)) = (&file_bytes, filename) {
            let file_dest_dir = sfp.clone();
            std::fs::create_dir_all(&file_dest_dir).ok();
            let actual_filename = get_unique_filename(&file_dest_dir, fname);
            let file_location = format!("{file_dest_dir}/{actual_filename}");
            std::fs::write(&file_location, bytes).ok();
            if open_file_on_launch {
                let container_file_path =
                    format!("{}/Desktop/files/{actual_filename}", settings.container_config_path);
                final_env.insert("VRECKAN_FILE".into(), container_file_path);
                launch_context = Some(serde_json::json!({ "type": "file", "value": fname }));
            }
        }
    }
    // `file_path` launches point `VRECKAN_FILE` at a server-side file via the
    // endpoint env; give them the same `launch_context` as the file-upload
    // path so session listings are consistent (ANT-234).
    if launch_context.is_none() {
        if let Some(fp) = final_env.get("VRECKAN_FILE").map(|v| v.as_str()) {
            let fname = std::path::Path::new(fp)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(fp);
            launch_context = Some(serde_json::json!({ "type": "file", "value": fname }));
        }
    }
    apply_external_mounts(&mut volumes, username, host_mount_path.as_deref());

    // --- Launch the container ----------------------------------------------
    let network = state.discovered_network.read().await.clone();

    let initial_tokens = if launch_in_room_mode {
        let mut ct = serde_json::Map::new();
        ct.insert("role".into(), Value::String("controller".into()));
        ct.insert("slot".into(), Value::Null);
        let mut obj = serde_json::Map::new();
        // The key is the controller token value itself (mirrors Python, where
        // the dict is `{ controller_token: {...} }`).
        obj.insert(controller_token.clone().unwrap_or_default(), Value::Object(ct));
        Some(Value::Object(obj))
    } else {
        // Plain session: the owner is addressed in the token table by the
        // session access token (the same mapping `push_token_table` uses for
        // a plain session), with first-class MK control. This is what keeps
        // the owner on a distinct token identity from any share-minted
        // visitor, so neither displaces the other on slot 1 (ANT-438).
        let mut ct = serde_json::Map::new();
        ct.insert("role".into(), Value::String("controller".into()));
        ct.insert("slot".into(), Value::Null);
        ct.insert("mk_control".into(), Value::Bool(true));
        let mut obj = serde_json::Map::new();
        obj.insert(access_token.clone(), Value::Object(ct));
        Some(Value::Object(obj))
    };

    // The launch parameters, stored in the session's container registry so a
    // missing container can be re-launched (recreate / relaunch after reboot).
    let mut launch_kwargs = serde_json::Map::new();
    launch_kwargs.insert("session_id".into(), Value::String(session_id.clone()));
    launch_kwargs.insert("env_vars".into(), serde_json::to_value(&final_env).unwrap_or_default());
    launch_kwargs.insert("volumes".into(), serde_json::to_value(&volumes).unwrap_or_default());
    launch_kwargs.insert("gpu_config".into(), serde_json::to_value(&gpu_config).unwrap_or_default());
    launch_kwargs.insert("network".into(), serde_json::to_value(&network).unwrap_or_default());
    launch_kwargs.insert("ephemeral".into(), Value::Bool(is_ephemeral_launch));
    if launch_in_room_mode {
        launch_kwargs.insert("is_collaboration".into(), Value::Bool(true));
    }
    // Both room and plain launches carry the master token + initial token
    // table so a recreate/relaunch (which replays `launch_kwargs`) restores
    // the session's secure mode (ANT-438).
    if let Some(mt) = &master_token {
        launch_kwargs.insert("master_token".into(), serde_json::to_value(mt).unwrap_or_default());
        if let Some(it) = &initial_tokens {
            launch_kwargs.insert("initial_tokens".into(), it.clone());
        }
    }

    // Test hook: when `fake_provider` is set, skip the real Docker daemon and
    // record the launch kwargs + app config (mirrors the Python FakeProvider).
    // The real branch keeps the provider alive so the launch-time capability
    // probe can exec into the container (ANT-438).
    let (instance_details, launch_digest, probe_provider) =
        if state.fake_provider.load(std::sync::atomic::Ordering::Relaxed) {
            let mut rec = serde_json::Map::new();
            rec.insert("app_config".into(), app_config_dict.clone());
            rec.insert("launch_kwargs".into(), Value::Object(launch_kwargs.clone()));
            *state
                .fake_provider_record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Value::Object(rec));
            (
                LaunchResult {
                    instance_id: "ctr-1".into(),
                    ip: "10.0.0.5".into(),
                    port: 3000,
                },
                None,
                None,
            )
        } else {
            let provider = match DockerProvider::new(app_config_dict.clone()) {
                Ok(p) => p,
                Err(e) => return Err((StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), e.detail)),
            };
            let result = provider
                .launch(
                    &session_id,
                    final_env,
                    volumes,
                    &prefix_map,
                    gpu_config.clone(),
                    network,
                    launch_in_room_mode,
                    master_token.clone(),
                    initial_tokens,
                    is_ephemeral_launch,
                )
                .await;
            let details = match result {
                Ok(d) => d,
                Err(e) => {
                    cleanup_ephemeral(&host_mount_path, &ephemeral_base);
                    return Err((
                        StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                        e.detail,
                    ));
                }
            };
            // Record the launch digest (best-effort) for out-of-date detection.
            let digest = provider.get_remote_image_digest(&app.provider_config.base.image).await;
            (details, digest, Some(provider))
        };

    // A launch pulls a missing image on demand; cache the image's metadata so
    // the installed-apps table shows the real image identity without admin
    // action (the Rust equivalent of the `initialize` launch-time pull,
    // ANT-269). The `fake_provider` hook is honored inside the helper.
    cache_local_image_metadata(state, &app.provider_config.base.image).await;

    // --- Record the session -------------------------------------------------
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut session = serde_json::json!({
        "instance_id": instance_details.instance_id,
        "ip": instance_details.ip,
        "port": instance_details.port,
        "created_at": now,
        "access_token": access_token,
        "provider_app_id": application_id,
        "username": username,
        "app_name": app.name,
        "app_logo": app.logo,
        "name": session_name,
        "host_mount_path": host_mount_path.clone(),
        "shared_files_path": shared_files_path.clone(),
        "launch_context": launch_context.clone(),
        "custom_user": custom_user.clone(),
        "password": password.clone(),
        "gpu_config": gpu_config.clone(),
        "wayland_mode": wayland_mode,
        "ephemeral": is_ephemeral_launch,
        "launch_image_digest": launch_digest,
        "container_registry": {
            application_id: {
                "instance_id": instance_details.instance_id,
                "ip": instance_details.ip,
                "port": instance_details.port,
                "app_id": application_id,
                "created_at": now,
                "app_config": app_config_dict.clone(),
                "launch_kwargs": Value::Object(launch_kwargs),
            }
        }
    });
    if launch_in_room_mode {
        session["is_collaboration"] = serde_json::json!(true);
        session["controller_token"] = serde_json::json!(controller_token);
        session["viewer_token"] = serde_json::json!(viewer_token);
    }
    if let Some(mt) = &master_token {
        session["master_token"] = serde_json::json!(mt);
        session["viewers"] = serde_json::json!([]);
    }

    // --- Launch-time capability probe (ANT-438) ------------------------------
    // Plain sessions enter selkies secure mode at launch: the master token
    // was in the container's boot env and the owner's controller entry is the
    // initial token table. The probe decides promote-vs-legacy-fallback for
    // containers whose catalog record is unknown, and records the detected
    // capability on the session. Room launches are already secure by
    // construction (the readiness gate requires the initial table push to be
    // accepted), so only the capability record is added.
    let probe = if launch_in_room_mode {
        SelkiesProbe::Secure { version: None }
    } else if let Some(p) = &probe_provider {
        probe_selkies_capability(state, p, &session, &session_id).await
    } else {
        // Test hook: a recorded probe result stands in for the live probe;
        // absent one, assume the 2.0.0-class happy path.
        state
            .fake_selkies_probe
            .read()
            .await
            .clone()
            .unwrap_or(SelkiesProbe::Secure { version: None })
    };
    let secure = probe.is_secure();
    session["selkies_capability"] = Value::String(if secure { "secure".into() } else { "legacy".into() });
    // Detected version wins; fall back to the catalog's declared
    // `selkiesVersion` (Phase 0 seed) for visibility.
    let detected = match &probe {
        SelkiesProbe::Secure { version } | SelkiesProbe::Legacy { version } => version.clone(),
    };
    session["selkies_version"] =
        serde_json::to_value(detected.or_else(|| app.provider_config.base.selkies_version.clone()))
            .unwrap_or(Value::Null);
    if secure {
        if !launch_in_room_mode {
            // The probe's table push may have raced selkies' own boot;
            // confirm the owner's entry actually landed. Even a final failure
            // keeps the session secure — the promotion is idempotent and is
            // re-pushed on the next share visit (ANT-305).
            let mut pushed = false;
            for _ in 0..5 {
                if crate::collaboration::push_token_table(state, &session_id, &session).await {
                    pushed = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            if !pushed {
                warn!("[{session_id}] Initial token table not accepted after retries; the session stays secure and the table will be re-pushed on the next share visit.");
            }
        }
        info!(
            "[{session_id}] Launch probe: selkies '{}' — secure mode at launch.",
            session["selkies_version"]
        );
    } else {
        // Pre-2.0 container (or its token API rejected the table): roll the
        // session back to the documented legacy path. The boot env's master
        // token is harmless — the pre-2.0 stack never reads it.
        session.as_object_mut().map(|o| o.remove("master_token"));
        session["viewers"] = Value::Array(vec![]);
        info!(
            "[{session_id}] Launch probe: selkies '{}' — legacy path.",
            session["selkies_version"]
        );
    }
    state.sessions.write().await.insert(session_id.clone(), session);
    save_sessions(state).await;

    info!(
        "[{session_id}] Session ready for {username}. Proxying to {}:{}.",
        instance_details.ip, instance_details.port
    );

    let session_url = if launch_in_room_mode {
        format!("/room/{session_id}?access_token={access_token}&token={}", controller_token.clone().unwrap_or_default())
    } else {
        owner_session_url(&session_id, &access_token)
    };

    Ok(LaunchResponse { session_url, session_id })
}

/// Resolve the autostart script content + subpath for a launch. Mirrors the
/// autostart block in `_launch_common`.
fn resolve_autostart(
    state: &AppState,
    app: &InstalledApp,
    wayland_mode: bool,
    session_id: &str,
) -> (Option<String>, Option<String>) {
    let pc = &app.provider_config.base;
    let (subpath, custom_b64) = if wayland_mode {
        (
            format!("{}/autostart", ".config/labwc"),
            pc.custom_autostart_wayland_script_b64.as_deref(),
        )
    } else {
        (
            format!("{}/autostart", ".config/openbox"),
            pc.custom_autostart_script_b64.as_deref(),
        )
    };
    let content = if let Some(b64) = custom_b64 {
        match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => {
                warn!("[{session_id}] Failed to decode custom autostart script: {e}");
                None
            }
        }
    } else if pc.autostart {
        let suffix = if wayland_mode { "-wayland" } else { "" };
        let cache_path = format!(
            "{}/{}/{}{}",
            state.settings.autostart_cache_path,
            app.id,
            app.provider_config.base.image,
            suffix
        );
        std::fs::read_to_string(&cache_path).ok().filter(|s| !s.is_empty())
    } else {
        None
    };
    (content, Some(subpath))
}

/// Copy a directory tree (best-effort, ignoring dangling symlinks). Mirrors
/// `_safe_copytree`.
fn copy_tree(src: &str, dst: &str) {
    let _ = copy_dir_recursive(src, dst);
}

fn copy_dir_recursive(src: &str, dst: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.to_string() + "/" + &entry.file_name().to_string_lossy();
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path().to_string_lossy(), &target)?;
        } else if file_type.is_symlink() {
            if let Ok(target_path) = std::fs::read_link(entry.path()) {
                let _ = std::os::unix::fs::symlink(target_path, &target);
            }
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Remove an ephemeral host mount path if it is under the ephemeral base.
fn cleanup_ephemeral(host_mount_path: &Option<String>, ephemeral_base: &str) {
    if let Some(p) = host_mount_path {
        if p.starts_with(ephemeral_base) {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

// ---------------------------------------------------------------------------
// File reassembly (for the upload-based launch endpoints)
// ---------------------------------------------------------------------------

/// Resolve an upload id to its on-disk directory, rejecting anything that
/// escapes the upload root. Mirrors `_validated_upload_path`.
pub fn validated_upload_path(settings: &Settings, upload_id: &str) -> Result<String, (StatusCode, String)> {
    let root = std::fs::canonicalize(&settings.upload_dir)
        .map_err(|_| (StatusCode::NOT_FOUND, "Upload session not found.".to_string()))?;
    let path = std::fs::canonicalize(root.join(upload_id))
        .map_err(|_| (StatusCode::NOT_FOUND, "Upload session not found.".to_string()))?;
    let root_str = root.to_string_lossy().into_owned();
    let path_str = path.to_string_lossy().into_owned();
    if path_str == root_str || !path_str.starts_with(&format!("{root_str}/")) {
        return Err((StatusCode::NOT_FOUND, "Upload session not found.".to_string()));
    }
    if !path.is_dir() {
        return Err((StatusCode::NOT_FOUND, "Upload session not found.".to_string()));
    }
    Ok(path_str)
}

/// Concatenate the uploaded chunks into a single temp file and return its
/// path. Mirrors `_reassemble_file`.
pub fn reassemble_file(
    settings: &Settings,
    upload_id: &str,
    total_chunks: i64,
) -> Result<String, (StatusCode, String)> {
    let upload_dir = validated_upload_path(settings, upload_id)?;
    for i in 0..total_chunks {
        let chunk_path = format!("{upload_dir}/chunk_{i}");
        if !std::path::Path::new(&chunk_path).exists() {
            return Err((StatusCode::BAD_REQUEST, format!("Missing chunk {i} for upload.")));
        }
    }
    let temp_path = format!("{}/{}-{}", settings.upload_dir, upload_id, random_uuid());
    let mut final_file = std::fs::File::create(&temp_path)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to reassemble file.".to_string()))?;
    for i in 0..total_chunks {
        let chunk_path = format!("{upload_dir}/chunk_{i}");
        let chunk_data = std::fs::read(&chunk_path)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to reassemble file.".to_string()))?;
        use std::io::Write;
        final_file
            .write_all(&chunk_data)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to reassemble file.".to_string()))?;
    }
    let _ = std::fs::remove_dir_all(&upload_dir);
    Ok(temp_path)
}

// ---------------------------------------------------------------------------
// Route handlers (the E2EE-encrypted catalog + launch endpoints)
// ---------------------------------------------------------------------------

/// `POST /api/applications` — the list of apps the caller may launch.
async fn get_applications(
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
    let apps = state.installed_apps.read().await.clone();
    let user_groups = user.record.groups.clone();
    let username = user.record.username.clone();
    let mut user_apps: Vec<Application> = Vec::new();
    for app in apps.values() {
        if user_can_access_app(app, &username, &user_groups) {
            user_apps.push(Application {
                id: app.id.clone(),
                name: app.name.clone(),
                logo: app.logo.clone(),
                r#type: app.provider_config.base.r#type.clone(),
                home_directories: app.home_directories,
                nvidia_support: app.provider_config.base.nvidia_support,
                dri3_support: app.provider_config.base.dri3_support,
                url_support: app.provider_config.base.url_support,
                extensions: app.provider_config.base.extensions.clone(),
                is_meta_app: app.is_meta_app,
                selkies_version: app.provider_config.base.selkies_version.clone(),
            });
        }
    }
    user_apps.sort_by_key(|a| a.name.to_lowercase());
    let value = serde_json::to_value(&user_apps).unwrap_or_default();
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `POST /api/launch/simple` — launch an app with no extra input.
async fn launch_simple(
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
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: LaunchRequestSimple = match serde_json::from_value(decrypted) {
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
    match launch_common(
        &state,
        &req.application_id,
        &user.record.username,
        &user.effective_settings,
        req.home_name.as_deref(),
        HashMap::new(),
        req.language.as_deref(),
        req.selected_gpu.as_deref(),
        None,
        None,
        false,
        None,
        req.launch_in_room_mode,
        req.wayland_mode,
        req.session_name.as_deref(),
        true,
    )
    .await
    {
        Ok(resp) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::to_value(&resp).unwrap_or_default()).await,
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

/// `POST /api/launch/url` — launch an app pointed at a URL.
async fn launch_url(
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
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: LaunchRequestURL = match serde_json::from_value(decrypted) {
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
    let mut env = HashMap::new();
    env.insert("VRECKAN_URL".into(), req.url);
    match launch_common(
        &state,
        &req.application_id,
        &user.record.username,
        &user.effective_settings,
        req.home_name.as_deref(),
        env,
        req.language.as_deref(),
        req.selected_gpu.as_deref(),
        None,
        None,
        false,
        None,
        req.launch_in_room_mode,
        req.wayland_mode,
        req.session_name.as_deref(),
        true,
    )
    .await
    {
        Ok(resp) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::to_value(&resp).unwrap_or_default()).await,
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

/// `POST /api/launch/file` — launch an app with an uploaded file.
async fn launch_file(
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
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: LaunchRequestFile = match serde_json::from_value(decrypted) {
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
    // Reassemble the uploaded chunks, read them, then delete the temp file.
    let (file_bytes, safe_filename) = match reassemble_file(&state.settings, &req.upload_id, req.total_chunks) {
        Ok(path) => {
            let bytes = std::fs::read(&path).ok();
            let _ = std::fs::remove_file(&path);
            (bytes, std::path::Path::new(&req.filename).file_name().and_then(|f| f.to_str()).unwrap_or(&req.filename).to_string())
        }
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    match launch_common(
        &state,
        &req.application_id,
        &user.record.username,
        &user.effective_settings,
        req.home_name.as_deref(),
        HashMap::new(),
        req.language.as_deref(),
        req.selected_gpu.as_deref(),
        file_bytes,
        Some(safe_filename.as_str()),
        req.open_file_on_launch,
        None,
        req.launch_in_room_mode,
        req.wayland_mode,
        req.session_name.as_deref(),
        true,
    )
    .await
    {
        Ok(resp) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::to_value(&resp).unwrap_or_default()).await,
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

/// `POST /api/launch/file_path` — launch an app pointed at a server-side file
/// (requires persistent storage).
async fn launch_file_path(
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
    // Persistent storage is required for this endpoint.
    if !user.effective_settings.get("persistent_storage").and_then(|v| v.as_bool()).unwrap_or(false) {
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
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: LaunchRequestFilePath = match serde_json::from_value(decrypted) {
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
    if let Some(hn) = &req.home_name {
        if hn.to_lowercase() == "cleanroom" {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "detail": "Cannot open a server-side file in 'Cleanroom' mode." }),
            )
            .await;
        }
    }
    let username = user.record.username.clone();
    let shared_files_path = format!("{}/{}/_vreckan_shared_files", state.settings.storage_path, username);
    let safe_filename = std::path::Path::new(&req.filename)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(&req.filename)
        .to_string();
    let file_location = format!("{shared_files_path}/{safe_filename}");
    if !std::path::Path::new(&file_location).exists() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("File '{safe_filename}' not found.") }),
        )
        .await;
    }
    let container_file_path = format!(
        "{}/Desktop/files/{safe_filename}",
        state.settings.container_config_path
    );
    let mut env = HashMap::new();
    env.insert("VRECKAN_FILE".into(), container_file_path);
    match launch_common(
        &state,
        &req.application_id,
        &username,
        &user.effective_settings,
        req.home_name.as_deref(),
        env,
        req.language.as_deref(),
        req.selected_gpu.as_deref(),
        None,
        None,
        false,
        None,
        req.launch_in_room_mode,
        req.wayland_mode,
        req.session_name.as_deref(),
        true,
    )
    .await
    {
        Ok(resp) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::to_value(&resp).unwrap_or_default()).await,
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

/// `POST /api/admin/status` — system stats, GPUs, and the caller's effective
/// settings/permissions.
async fn admin_status(
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
    let stats = get_system_stats(&state.settings);
    let is_admin = user.is_admin();
    let gpu_enabled = user
        .effective_settings
        .get("gpu")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let gpus_value = if gpu_enabled {
        let gpus = state.available_gpus.read().await.clone();
        serde_json::to_value(&gpus).unwrap_or_default()
    } else {
        serde_json::json!([])
    };
    let mut response = serde_json::json!({
        "is_admin": is_admin,
        "username": user.record.username,
        "settings": user.effective_settings,
        "gpus": gpus_value,
        "permissions": user.permissions,
        "roles": user.record.roles,
        "groups": user.record.groups,
    });
    if let (Some(resp_obj), Some(stats_obj)) = (response.as_object_mut(), stats.as_object()) {
        for (k, v) in stats_obj {
            resp_obj.insert(k.clone(), v.clone());
        }
    }
    if let Ok(Some(row)) = entities::app_setting::Entity::find_by_id("global_default_gpu".to_string())
        .one(&state.db.conn)
        .await
    {
        response["global_default_gpu"] = serde_json::json!(row.value);
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, response).await
}

/// `GET /api/app_icon/{app_id}` — a custom-uploaded app icon, base64-encoded.
async fn get_app_icon(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(app_id): Path<String>,
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
    let _ = &user;
    if !app_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "Invalid application ID." }),
        )
        .await;
    }
    let icon_path = format!("{}/{}.png", state.settings.app_icons_path, app_id);
    if !std::path::Path::new(&icon_path).starts_with(&state.settings.app_icons_path) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Access denied." }),
        )
        .await;
    }
    if !std::path::Path::new(&icon_path).exists() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Icon not found." }),
        )
        .await;
    }
    match std::fs::read(&icon_path) {
        Ok(data) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::OK,
                serde_json::json!({ "icon_data_b64": b64 }),
            )
            .await
        }
        Err(e) => {
            warn!("Failed to read or encode icon for app {app_id}: {e}");
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": "Error retrieving icon." }),
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

/// True when the session's container was launched from an older image than the
/// one currently pulled locally. Mirrors `_session_out_of_date`.
pub async fn session_out_of_date(state: &AppState, session_data: &Value) -> bool {
    let launch_digest = session_data
        .get("launch_image_digest")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(launch_digest) = launch_digest else {
        return false;
    };
    let Some(app_id) = session_data.get("provider_app_id").and_then(|v| v.as_str()) else {
        return false;
    };
    let apps = state.installed_apps.read().await;
    let Some(app) = apps.get(app_id) else {
        return false;
    };
    let image_name = app.provider_config.base.image.clone();
    let provider = match DockerProvider::new(serde_json::to_value(app).unwrap_or_default()) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let Some(local_info) = provider.get_local_image_info(&image_name).await else {
        return false;
    };
    let local_digests = local_info
        .get("digests")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    !local_digests.iter().any(|d| d.as_str().map(|s| s.contains(&launch_digest)).unwrap_or(false))
}

/// Stop a session's container(s) and clean up ephemeral storage. Mirrors
/// `_stop_session`.
pub async fn stop_session(state: &AppState, session_id: &str) {
    info!("[{session_id}] Stopping session...");
    // Prune the per-session in-memory maps the plain-session token-table
    // refresh keeps, or they grow one entry per session for the process
    // lifetime (ANT-470 F5). Done before the found-check so a double-stop or
    // a stale entry still gets cleaned up.
    state.selkies_client_conns.write().await.remove(session_id);
    state
        .last_pushed_token_tables
        .write()
        .await
        .remove(session_id);
    state
        .token_table_generations
        .write()
        .await
        .remove(session_id);
    let session_data = state.sessions.write().await.remove(session_id);
    let Some(session_data) = session_data else {
        warn!("Attempted to stop session {session_id}, but it was not found.");
        return;
    };
    // Tell any collaboration-room viewers the session has ended so their room
    // UI shows "session ended" instead of a dead iframe. A no-op for
    // non-collaboration sessions (they have no room connections).
    crate::collaboration::notify_session_ended(state, session_id).await;
    let registry = session_data
        .get("container_registry")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let registry_map = registry.as_object().cloned().unwrap_or_default();
    for (app_id, container_info) in registry_map.iter() {
        let Some(instance_id) = container_info.get("instance_id").and_then(|v| v.as_str()) else {
            continue;
        };
        // The stop path only needs the daemon connection, so fall back to an
        // empty provider when the app was uninstalled since launch — without
        // this the container would be orphaned (ANT-378).
        let provider = match state.installed_apps.read().await.get(app_id).cloned() {
            Some(app) => DockerProvider::new(serde_json::to_value(&app).unwrap_or_default()),
            None => DockerProvider::new(serde_json::json!({})),
        };
        if let Ok(provider) = provider {
            provider.stop(instance_id).await;
        }
    }
    save_sessions(state).await;
    let ephemeral_base = format!("{}/vreckan_ephemeral", state.settings.storage_path);
    if let Some(hmp) = session_data.get("host_mount_path").and_then(|v| v.as_str()) {
        if hmp.starts_with(&ephemeral_base) && std::path::Path::new(hmp).exists() {
            let _ = std::fs::remove_dir_all(hmp);
        }
    }
    if let Some(sfp) = session_data
        .get("shared_files_path")
        .and_then(|v| v.as_str())
    {
        if sfp.starts_with(&ephemeral_base) && std::path::Path::new(sfp).exists() {
            let _ = std::fs::remove_dir_all(sfp);
            info!("[{session_id}] Removed ephemeral shared files directory.");
        }
    }
    info!("[{session_id}] Session stopped and cleaned up successfully.");
}

/// Whether a session was launched with ephemeral (cleanroom) storage.
/// Prefers the explicit flag recorded at launch and falls back to the host
/// mount path for sessions saved before the flag existed (ANT-378).
fn session_is_ephemeral(session_data: &Value, ephemeral_base: &str) -> bool {
    if let Some(v) = session_data.get("ephemeral").and_then(|v| v.as_bool()) {
        return v;
    }
    session_data
        .get("host_mount_path")
        .and_then(|v| v.as_str())
        .is_some_and(|p| p.starts_with(ephemeral_base))
}

/// Stop + remove the containers of every ephemeral session and delete their
/// ephemeral directories (ANT-378). Ephemeral containers use the `no`
/// restart policy, but the daemon still resurrects a container that was
/// *running* when it restarted — so the server reaps them itself: on
/// graceful shutdown (SIGTERM/SIGINT) and on startup, where the same sweep
/// also collects containers orphaned by a previous run that was hard-killed
/// (kill -9). Best effort: a down Docker daemon is logged, never fatal. The
/// session rows are kept so users can recreate them (the launch kwargs are
/// persisted in the container registry).
pub async fn reap_ephemeral_sessions(state: &AppState) {
    let ephemeral_base = format!("{}/vreckan_ephemeral", state.settings.storage_path);
    let sessions = state.sessions.read().await.clone();
    for (session_id, session_data) in sessions.iter() {
        if !session_is_ephemeral(session_data, &ephemeral_base) {
            continue;
        }
        let registry = session_data
            .get("container_registry")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        for (app_id, container_info) in registry.as_object().cloned().unwrap_or_default() {
            let Some(instance_id) = container_info.get("instance_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let provider = match container_info.get("app_config") {
                Some(cfg) => DockerProvider::new(cfg.clone()),
                None => DockerProvider::new(serde_json::json!({})),
            };
            match provider {
                Ok(p) => {
                    info!(
                        "[{session_id}] Reaping ephemeral container {} (app '{app_id}').",
                        &instance_id[..instance_id.len().min(12)]
                    );
                    p.stop(instance_id).await;
                }
                Err(e) => {
                    warn!(
                        "[{session_id}] Docker daemon unreachable at reap time; \
                         ephemeral container {} left for the daily sweep: {}",
                        &instance_id[..instance_id.len().min(12)],
                        e.detail
                    );
                }
            }
        }
        for key in ["host_mount_path", "shared_files_path"] {
            let Some(p) = session_data.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            if p.starts_with(&ephemeral_base) && std::path::Path::new(p).exists() {
                let _ = std::fs::remove_dir_all(p);
            }
        }
        info!("[{session_id}] Reaped ephemeral session storage.");
    }
}

/// Recreate a session's container(s) from the current image. Mirrors
/// `recreate_session`. Returns the updated session data.
pub async fn recreate_session(
    state: &AppState,
    session_id: &str,
) -> Result<Value, (StatusCode, String)> {
    let mut sessions = state.sessions.write().await;
    let Some(session_data) = sessions.get_mut(session_id) else {
        return Err((StatusCode::NOT_FOUND, "Session not found.".to_string()));
    };
    let registry = session_data
        .get("container_registry")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if registry.as_object().map(|o| o.is_empty()).unwrap_or(true)
        && session_data.get("provider_app_id").is_some()
    {
        // Fallback registry from the top-level fields.
        let app_id = session_data["provider_app_id"].as_str().unwrap_or_default();
        let instance_id = session_data["instance_id"].clone();
        session_data["container_registry"] = serde_json::json!({
            app_id: { "instance_id": instance_id }
        });
    }
    let registry = session_data
        .get("container_registry")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if registry.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Session has no containers to recreate.".to_string(),
        ));
    }
    let primary_app_id = session_data
        .get("provider_app_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let registry_map = registry.as_object().cloned().unwrap_or_default();
    let prefix_map = state.path_prefix_map.read().await.clone();
    let mut primary_restored = false;
    for (app_id, container_info) in registry_map.iter() {
        let Some(app_config) = container_info.get("app_config") else {
            continue;
        };
        let Some(launch_kwargs) = container_info.get("launch_kwargs") else {
            continue;
        };
        let instance_id = container_info
            .get("instance_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // Stop + remove the old container first.
        if let Some(instance_id) = &instance_id {
            if let Ok(provider) = DockerProvider::new(app_config.clone()) {
                provider.stop(instance_id).await;
            }
        }
        // Re-launch from the current image with the original parameters.
        let provider = match DockerProvider::new(app_config.clone()) {
            Ok(p) => p,
            Err(e) => {
                warn!("[{session_id}] Failed to recreate container for app '{app_id}': {}", e.detail);
                continue;
            }
        };
        let env_vars: HashMap<String, String> = launch_kwargs
            .get("env_vars")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let volumes: HashMap<String, VolumeSpec> = launch_kwargs
            .get("volumes")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let gpu_config: Option<GpuInfo> = launch_kwargs
            .get("gpu_config")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok());
        let network: Option<String> = launch_kwargs
            .get("network")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok());
        let is_collaboration = launch_kwargs
            .get("is_collaboration")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let ephemeral = launch_kwargs
            .get("ephemeral")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let master_token: Option<String> = launch_kwargs
            .get("master_token")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok());
        let initial_tokens = launch_kwargs.get("initial_tokens").cloned();
        if ephemeral {
            // The ephemeral home directory may have been cleaned up while the
            // container was gone (stop/recreate gap); Docker rejects a bind
            // whose source path is missing, so recreate it first (ANT-378).
            let ephemeral_base = format!("{}/vreckan_ephemeral", state.settings.storage_path);
            for host in volumes.keys() {
                if host.starts_with(&ephemeral_base) {
                    std::fs::create_dir_all(host).ok();
                }
            }
        }
        match provider
            .launch(
                session_id,
                env_vars,
                volumes,
                &prefix_map,
                gpu_config,
                network,
                is_collaboration,
                master_token,
                initial_tokens,
                ephemeral,
            )
            .await
        {
            Ok(details) => {
                if let Some(reg_obj) = session_data.get_mut("container_registry").and_then(|r| r.as_object_mut()) {
                    if let Some(ci) = reg_obj.get_mut(app_id) {
                        ci["instance_id"] = serde_json::json!(details.instance_id);
                        ci["ip"] = serde_json::json!(details.ip);
                        ci["port"] = serde_json::json!(details.port);
                    }
                }
                if Some(app_id.as_str()) == primary_app_id.as_deref() {
                    session_data["instance_id"] = serde_json::json!(details.instance_id);
                    session_data["ip"] = serde_json::json!(details.ip);
                    session_data["port"] = serde_json::json!(details.port);
                    primary_restored = true;
                }
                info!(
                    "[{session_id}] Recreated container for app '{app_id}' ({}).",
                    details.instance_id
                );
            }
            Err(e) => {
                warn!("[{session_id}] Failed to recreate container for app '{app_id}': {}", e.detail);
            }
        }
    }
    if !primary_restored {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not recreate the session's primary container.".to_string(),
        ));
    }
    // Refresh the recorded digest so the session is no longer out of date.
    if let Some(app_id) = &primary_app_id {
        if let Some(app) = state.installed_apps.read().await.get(app_id) {
            let provider = DockerProvider::new(serde_json::to_value(app).unwrap_or_default());
            if let Ok(p) = provider {
                if let Some(digest) = p
                    .get_remote_image_digest(&app.provider_config.base.image)
                    .await
                {
                    session_data["launch_image_digest"] = serde_json::json!(digest);
                }
            }
        }
    }
    let updated = session_data.clone();
    drop(sessions);
    save_sessions(state).await;
    Ok(updated)
}

// ---------------------------------------------------------------------------
// Session route handlers
// ---------------------------------------------------------------------------

/// `GET /api/sessions` — the caller's view of all active sessions, newest
/// first.
///
/// Since ANT-440 the list includes other users' running sessions (not just
/// the caller's own), each annotated with `username`, `is_mine`, and
/// `joinable` so the UI can offer a Join action. Since ANT-472 an other
/// user's plain session is `joinable` only when its owner has expressly
/// granted the caller access (a non-revoked, non-expired named share link,
/// or a persisted participant entry); the UI hides the other rows and the
/// app-card Join affordance follows this flag. `session_url` carries the
/// caller's own token and is populated only for their own sessions.
async fn get_my_sessions(
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
    let username = user.record.username.clone();
    let user_groups = user.record.groups.clone();
    // Snapshot the installed-apps table once (the joinable check resolves
    // each session's app access against it).
    let installed_apps = state.installed_apps.read().await.clone();
    let sessions = state.sessions.read().await.clone();
    let mut user_sessions: Vec<ActiveSessionInfo> = Vec::new();
    for (sid, s_data) in sessions.iter() {
        let owner = s_data
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let is_mine = owner == username;
        let out_of_date = session_out_of_date(&state, s_data).await;
        let is_collaboration = s_data
            .get("is_collaboration")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // The caller's own sessions get a direct-open URL with their own
        // token; other users' sessions are entered through a Join.
        let session_url = if is_mine {
            if is_collaboration {
                let token = s_data
                    .get("controller_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                Some(format!("/room/{sid}?token={token}"))
            } else {
                let token = s_data
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                Some(owner_session_url(sid, token))
            }
        } else {
            None
        };
        let app_id = s_data
            .get("provider_app_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Joinable: the caller's own sessions keep the app-access check (the
        // UI offers Open on them). For another user's plain session the
        // owner's express grant is the sole gate (ANT-472): a non-revoked,
        // non-expired named share link for the caller, or a persisted
        // participant entry (grandfathered v0.8.4 self-serve joiners
        // included). Room sessions have their own invite flow.
        let joinable = if is_mine {
            installed_apps
                .get(app_id)
                .map(|app| user_can_access_app(app, &username, &user_groups))
                .unwrap_or(false)
        } else if is_collaboration {
            false
        } else {
            session_shares::caller_has_session_grant(&state, sid, s_data, &username).await
        };
        // F3 (ANT-468, ANT-444 security report): a file/URL `launch_context`
        // (and the session's custom `name`) can reveal what the owner launched.
        // The UI hides rows the caller can't join, but the API still returns
        // them — so null out both for other users' non-joinable sessions. Own
        // sessions stay fully populated; sessions the caller can join keep
        // them (the caller can open the app anyway, so the context is not
        // sensitive there).
        let reveal_context = is_mine || joinable;
        user_sessions.push(ActiveSessionInfo {
            session_id: sid.clone(),
            app_id: app_id.to_string(),
            app_name: s_data.get("app_name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            app_logo: s_data.get("app_logo").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            created_at: s_data.get("created_at").and_then(|v| v.as_f64()).unwrap_or(0.0),
            session_url,
            launch_context: if reveal_context {
                s_data.get("launch_context").cloned()
            } else {
                Some(Value::Null)
            },
            is_collaboration,
            name: if reveal_context {
                s_data.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            },
            out_of_date,
            username: owner,
            is_mine,
            joinable,
            participants: if is_mine {
                session_shares::participant_usernames(s_data)
            } else {
                Vec::new()
            },
        });
    }
    user_sessions.sort_by(|a, b| b.created_at.partial_cmp(&a.created_at).unwrap_or(std::cmp::Ordering::Equal));
    let value = serde_json::to_value(&user_sessions).unwrap_or_default();
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `GET /api/sessions/{session_id}/status` — readiness probe for a session.
async fn get_session_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
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
    let session_data = state.sessions.read().await.get(&session_id).cloned();
    let Some(session_data) = session_data else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    };
    if session_data.get("username").and_then(|v| v.as_str()) != Some(user.record.username.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    }
    let ip = session_data.get("ip").and_then(|v| v.as_str()).map(|s| s.to_string());
    let port = session_data.get("port").and_then(|v| v.as_i64()).map(|p| p as u16);
    let (ready, detail) = match (ip, port) {
        (Some(ip), Some(port)) => {
            let url = format!("http://{ip}:{port}/");
            match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
            {
                Ok(client) => match client.get(&url).send().await {
                    Ok(resp) => {
                        let code = resp.status().as_u16();
                        if code < 500 {
                            (true, format!("HTTP {code}"))
                        } else {
                            (false, format!("HTTP {code}"))
                        }
                    }
                    Err(e) => (false, format!("unreachable ({})", e)),
                },
                Err(e) => (false, format!("unreachable ({e})")),
            }
        }
        _ => (false, "No upstream address recorded.".to_string()),
    };
    let value = serde_json::json!({ "ready": ready, "detail": detail });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

/// `DELETE /api/sessions/{session_id}` — stop the caller's session.
async fn stop_my_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
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
    // F4-followup (ANT-444 / ANT-471): the stop route authenticated with the
    // SameSite=Lax login cookie but never read its E2EE body, so a cross-site
    // top-level form POST / DELETE from a logged-in victim could stop their own
    // sessions (the response body is E2EE-encrypted, so an attacker learns only
    // the status). Requiring the AES-256-GCM request body is the CSRF gate: the
    // normal client flow (secureFetch with an encrypted empty body) passes, a
    // cross-site form POST / XHR is rejected 400 before anything changes. The
    // decrypted body is unused (the client sends an empty object).
    let _ = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d }))
                .await
        }
    };
    let session_data = state.sessions.read().await.get(&session_id).cloned();
    let Some(session_data) = session_data else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    };
    if session_data.get("username").and_then(|v| v.as_str()) != Some(user.record.username.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    }
    stop_session(&state, &session_id).await;
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, serde_json::json!({})).await
}

/// `POST /api/sessions/{session_id}/recreate` — recreate the session's
/// container(s) from the current image.
async fn recreate_my_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
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
    // F4-followup (ANT-444 / ANT-471): the recreate route authenticated with the
    // SameSite=Lax login cookie but never read its E2EE body, so a cross-site
    // top-level form POST from a logged-in victim could recreate their sessions
    // (the response body is E2EE-encrypted, so an attacker learns only the
    // status). Requiring the AES-256-GCM request body is the CSRF gate: the
    // normal client flow (secureFetch with an encrypted empty body) passes, a
    // cross-site form POST / XHR is rejected 400 before anything changes. The
    // decrypted body is unused (the client sends an empty object).
    let _ = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d }))
                .await
        }
    };
    let session_data = state.sessions.read().await.get(&session_id).cloned();
    let Some(session_data) = session_data else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    };
    if session_data.get("username").and_then(|v| v.as_str()) != Some(user.record.username.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    }
    match recreate_session(&state, &session_id).await {
        Ok(updated) => {
            let is_collaboration = updated
                .get("is_collaboration")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let session_url = if is_collaboration {
                let token = updated
                    .get("controller_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                format!("/room/{session_id}?token={token}")
            } else {
                let token = updated
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                owner_session_url(&session_id, token)
            };
            let value = serde_json::json!({ "session_id": session_id, "session_url": session_url });
            e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
        }
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

/// `POST /api/sessions/{session_id}/join` — self-serve join (ANT-440): mint
/// the caller their own participant token into a running plain session and
/// return the session URL that admits them. No share link or owner URL
/// needed. Idempotent per (session, user): re-joining returns the caller's
/// existing participant token.
///
/// Statuses: 400 the E2EE body gate rejected the request (F4); 404 the
/// session is gone; 409 it is a room session (rooms use their own invite
/// flow); 403 the session's owner has not granted the caller access
/// (a named share link, or a persisted participant entry — ANT-472); 500
/// the mint failed.
async fn join_my_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
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
    // F4 (ANT-444 / ANT-469): the web client authenticates with the HttpOnly
    // SameSite=Lax login cookie and the app has no CORS layer, so a cross-site
    // top-level form POST reaches this route fully authenticated and would
    // otherwise mint the victim a participant token. A form cannot set the
    // X-Session-ID header or carry an AES-256-GCM body, so requiring the E2EE
    // request body is the CSRF gate: the normal client flow (secureFetch with
    // an encrypted body) passes, a cross-site form POST / XHR is rejected 400
    // before any token is minted. The decrypted body is unused (the client
    // sends an empty object).
    let _ = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d }))
                .await
        }
    };
    let session_data = state.sessions.read().await.get(&session_id).cloned();
    let Some(session_data) = session_data else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    };
    // Room sessions use their own invite flow: v1 defers joining a room
    // (it would mint a participant into the room's MK/slot model).
    if session_data
        .get("is_collaboration")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::CONFLICT,
            serde_json::json!({ "detail": "Room sessions use their own invite flow." }),
        )
        .await;
    }
    // The owner's express grant is the sole join gate (ANT-472): a
    // non-revoked, non-expired named share link for the caller, or a
    // persisted participant entry (grandfathered v0.8.4 self-serve joiners
    // included). The per-app access check no longer blocks a join — the
    // owner may invite a user with no entry in the app's allowlist.
    let granted = session_shares::caller_has_session_grant(
        &state,
        &session_id,
        &session_data,
        &user.record.username,
    )
    .await;
    if !granted {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "The session owner has not granted you access." }),
        )
        .await;
    }
    match session_shares::grant_session_join_access(&state, &session_id, &user.record.username)
        .await
    {
        Some(session_url) => {
            // Report the mode the session is in after the grant so the UI
            // can surface the WebSockets single-controller tradeoff.
            let secure = state
                .sessions
                .read()
                .await
                .get(&session_id)
                .and_then(|s| s.get("master_token").and_then(|v| v.as_str()))
                .map(|m| !m.is_empty())
                .unwrap_or(false);
            let value = serde_json::json!({
                "session_url": session_url,
                "join_mode": if secure { "secure" } else { "legacy" },
            });
            e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
        }
        None => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": "Failed to join the session." }),
        )
        .await,
    }
}

/// `POST /api/sessions/{session_id}/send_file` — send an uploaded file into a
/// running session's storage.
async fn send_file_to_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
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
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: SendFileToSessionRequest = match serde_json::from_value(decrypted) {
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
    let session_data = state.sessions.read().await.get(&session_id).cloned();
    let Some(session_data) = session_data else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    };
    if session_data.get("username").and_then(|v| v.as_str()) != Some(user.record.username.as_str()) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Session not found or permission denied." }),
        )
        .await;
    }
    let Some(host_mount_path) = session_data.get("host_mount_path").and_then(|v| v.as_str()) else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "Cannot send files to this session as it has no mounted storage." }),
        )
        .await;
    };
    let reassembled = match reassemble_file(&state.settings, &req.upload_id, req.total_chunks) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let safe_filename = std::path::Path::new(&req.filename)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(&req.filename)
        .to_string();
    let ephemeral_base = format!("{}/vreckan_ephemeral", state.settings.storage_path);
    let is_persistent = !host_mount_path.starts_with(&ephemeral_base);
    let file_dest_dir = if is_persistent {
        format!(
            "{}/{}/_vreckan_shared_files",
            state.settings.storage_path, user.record.username
        )
    } else {
        format!("{host_mount_path}/Desktop/files")
    };
    let _ = std::fs::create_dir_all(&file_dest_dir);
    let actual_filename = get_unique_filename(&file_dest_dir, &safe_filename);
    let file_location = format!("{file_dest_dir}/{actual_filename}");
    if let Err(e) = std::fs::rename(&reassembled, &file_location) {
        let _ = std::fs::remove_file(&reassembled);
        warn!("Failed to move reassembled file to session storage: {e}");
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": "Could not place file in session storage." }),
        )
        .await;
    }
    info!(
        "[{session_id}] User '{}' wrote file '{actual_filename}' (as '{safe_filename}') to session.",
        user.record.username
    );
    let value = serde_json::json!({
        "status": "success",
        "message": format!("File '{safe_filename}' sent to session."),
    });
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, value).await
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// The E2EE-encrypted catalog + launch + session router. Mirrors
/// `encrypted_router` and `session_router`. Mounted under `/api` (see
/// `app.rs`).
pub fn catalog_router() -> Router<AppState> {
    Router::new()
        .route("/applications", post(get_applications))
        .route("/launch/simple", post(launch_simple))
        .route("/launch/url", post(launch_url))
        .route("/launch/file", post(launch_file))
        .route("/launch/file_path", post(launch_file_path))
        .route("/admin/status", post(admin_status))
        .route("/app_icon/{app_id}", get(get_app_icon))
        .route("/sessions", get(get_my_sessions))
        .route("/sessions/{session_id}/status", get(get_session_status))
        .route("/sessions/{session_id}", delete(stop_my_session))
        .route("/sessions/{session_id}/recreate", post(recreate_my_session))
        .route("/sessions/{session_id}/join", post(join_my_session))
        .route("/sessions/{session_id}/send_file", post(send_file_to_session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_standard() {
        assert_eq!(
            parse_selkies_version("selkies 2.0.0"),
            Some((2, "2.0.0".to_string()))
        );
        assert_eq!(
            parse_selkies_version("selkies 1.4.3\n"),
            Some((1, "1.4.3".to_string()))
        );
        assert_eq!(
            parse_selkies_version("SELKIES_RELEASE=2.1.2 selkies 2.1.2"),
            Some((2, "2.1.2".to_string()))
        );
    }

    #[test]
    fn parse_version_absent_or_garbage() {
        assert_eq!(parse_selkies_version(""), None);
        assert_eq!(parse_selkies_version("/usr/bin/selkies: not found"), None);
        assert_eq!(parse_selkies_version("No such file or directory"), None);
    }

    #[test]
    fn classify_probe_json_is_secure() {
        assert_eq!(
            classify_sessions_probe(200, Some("application/json")),
            Some(SelkiesProbe::Secure { version: None })
        );
        assert_eq!(
            classify_sessions_probe(401, Some("application/json; charset=utf-8")),
            Some(SelkiesProbe::Secure { version: None })
        );
    }

    #[test]
    fn classify_probe_401_plain_is_secure() {
        // selkies' own secure-mode 401 (basic auth passed, session token
        // missing) answers "Unauthorized" as plain text — that is the
        // definitive secure-mode signal.
        assert_eq!(
            classify_sessions_probe(401, Some("text/plain; charset=utf-8")),
            Some(SelkiesProbe::Secure { version: None })
        );
        assert_eq!(
            classify_sessions_probe(401, Some("text/plain")),
            Some(SelkiesProbe::Secure { version: None })
        );
    }

    #[test]
    fn classify_probe_html_is_ambiguous() {
        // nginx answered instead of selkies: either the pre-2.0 SPA
        // fallback or a transient startup error page — let the exec
        // arbitrate (a 5xx HTML page must not permanently mark a 2.x
        // container legacy).
        assert_eq!(
            classify_sessions_probe(200, Some("text/html; charset=utf-8")),
            None
        );
        assert_eq!(classify_sessions_probe(404, Some("text/html")), None);
        assert_eq!(classify_sessions_probe(502, Some("text/html")), None);
    }

    #[test]
    fn classify_probe_other_is_ambiguous() {
        assert_eq!(classify_sessions_probe(200, None), None);
        // A non-401 plain body is not selkies' secure-mode 401.
        assert_eq!(classify_sessions_probe(502, Some("text/plain")), None);
        assert_eq!(classify_sessions_probe(200, Some("text/plain")), None);
    }
}
