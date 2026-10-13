//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tests for the admin app-template and template-schema endpoints.
//!
//! Ports `tests/test_templates`. Two related but distinct concepts:
//!
//! * **Templates** (`/api/admin/apps/templates`) are named bundles of setting
//!   overrides that admins apply to installed apps. Built-in templates ship as
//!   YAML files under `default_app_templates_path`; user templates live in the
//!   database and may override a built-in by sharing its name. In the test
//!   environment the default path does not exist, so the app starts with a
//!   single blank "Default" template — the `builtin_templates` helper points
//!   the path at a temp dir with a real built-in to exercise the
//!   protection/override/restore logic.
//!
//! * **The schema** (`/api/admin/apps/templates/schema`) is the base layer
//!   describing every setting the template editor can show. It is seeded from
//!   the bundled `template_schema.yml` at startup and can be replaced
//!   wholesale through the save endpoint.

mod common;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use sea_orm::{EntityTrait};
use serde_json::json;
use vreckan::catalog;

/// Point the built-in template dir at a temp dir with one template, then
/// (re)load the templates so the built-in is in place. Mirrors the
/// `builtin_templates` fixture (which monkeypatches the path before the app's
/// lifespan loads templates at startup). Because `build_state` already seeded
/// a blank "Default" template into the DB, we delete it first so the reload
/// reflects the "built-in present at startup" scenario (no "Default" row).
async fn builtin_templates(sc: &mut SecureClient) {
    let dir = std::env::temp_dir().join(format!(
        "vreckan-rust-builtin-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // Filename must match the endpoint's convention:
    // name.lower().replace(" ", "_") + ".yml"
    std::fs::write(
        dir.join("built-in.yml"),
        serde_yaml::to_string(&json!({ "name": "Built-In", "settings": { "TITLE": "Built In" } }))
            .unwrap(),
    )
    .unwrap();
    // Remove the seeded "Default" template so the reload only sees the
    // built-in (mirrors a fresh startup where the built-in dir is populated).
    if let Ok(row) = vreckan::entities::app_template::Entity::find_by_id("Default".to_string())
        .one(&sc.state.db.conn)
        .await
    {
        if let Some(row) = row {
            let am = vreckan::entities::app_template::ActiveModel {
                name: sea_orm::ActiveValue::Set(row.name),
                settings: sea_orm::ActiveValue::NotSet,
            };
            let _ = vreckan::entities::app_template::Entity::delete(am)
                .exec(&sc.state.db.conn)
                .await;
        }
    }
    sc.state.settings.default_app_templates_path = dir.to_string_lossy().to_string();
    catalog::load_app_templates(&sc.state).await;
    // The router holds a *clone* of the state, so its `settings` (and thus the
    // DELETE handler's `default_app_templates_path`) still points at the
    // original path. Rebuild the router from the updated state so the handlers
    // see the new path (preserving the auth cookie from the earlier login).
    let cookie = sc.http.auth_cookie.clone();
    sc.http = common::TestClient::new(sc.state.clone());
    sc.http.auth_cookie = cookie;
}

// ---------------------------------------------------------------------------
// Template CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_templates_default() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // No built-in templates exist in the test env, so startup creates a
    // single blank "Default" template in the database.
    let (status, body) = sc.call("GET", "/api/admin/apps/templates", None).await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    assert_eq!(body, Some(json!([ { "name": "Default", "settings": {} }])));
}

#[tokio::test]
async fn test_create_template() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates",
            Some(&json!({ "name": "My Template", "settings": { "TITLE": "X" } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{:?}", body);
    assert_eq!(body, Some(json!({ "name": "My Template", "settings": { "TITLE": "X" } })));
    let (status, templates) = sc.call("GET", "/api/admin/apps/templates", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = templates
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"My Template"));
    assert!(names.contains(&"Default"));
}

#[tokio::test]
async fn test_create_template_invalid_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates",
            Some(&json!({ "name": "bad name!", "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "Invalid template name.");
}

#[tokio::test]
async fn test_create_template_malformed_body() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/apps/templates", Some(&json!({ "settings": {} })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains("name"));
}

#[tokio::test]
async fn test_create_template_overrides_existing() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/templates",
            Some(&json!({ "name": "My Template", "settings": { "TITLE": "One" } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates",
            Some(&json!({ "name": "My Template", "settings": { "TITLE": "Two" } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{:?}", body);
    assert_eq!(body.as_ref().unwrap()["settings"], json!({ "TITLE": "Two" }));
    let (_status, templates) = sc.call("GET", "/api/admin/apps/templates", None).await;
    let entry = templates
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "My Template")
        .unwrap();
    assert_eq!(entry["settings"], json!({ "TITLE": "Two" }));
}

#[tokio::test]
async fn test_delete_template_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc.call("DELETE", "/api/admin/apps/templates/Ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Template 'Ghost' not found.");
}

#[tokio::test]
async fn test_delete_user_template() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/templates",
            Some(&json!({ "name": "My Template", "settings": { "TITLE": "X" } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc
        .call("DELETE", "/api/admin/apps/templates/My%20Template", None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_status, templates) = sc.call("GET", "/api/admin/apps/templates", None).await;
    let names: Vec<&str> = templates
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Default"]);
}

// ---------------------------------------------------------------------------
// Built-in template protection, override, and restore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_templates_with_builtin() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    builtin_templates(&mut sc).await;
    // With a built-in template present at startup, no blank "Default"
    // template is created.
    let (status, body) = sc.call("GET", "/api/admin/apps/templates", None).await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    assert_eq!(
        body,
        Some(json!([ { "name": "Built-In", "settings": { "TITLE": "Built In" } } ]))
    );
}

#[tokio::test]
async fn test_delete_builtin_template_protected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    builtin_templates(&mut sc).await;
    let (status, body) = sc.call("DELETE", "/api/admin/apps/templates/Built-In", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body.unwrap()["detail"],
        "Cannot delete the default template 'Built-In'. You can override it by creating a new template with the same name."
    );
}

#[tokio::test]
async fn test_builtin_template_override_and_restore() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    builtin_templates(&mut sc).await;
    // A pure built-in cannot be deleted ...
    let (status, _body) = sc.call("DELETE", "/api/admin/apps/templates/Built-In", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // ... but it can be overridden by a user template with the same name.
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/templates",
            Some(&json!({ "name": "Built-In", "settings": { "TITLE": "Overridden" } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Once a user override exists, deletion is allowed — and the built-in
    // (from the bundled file) takes effect again.
    let (status, _) = sc.call("DELETE", "/api/admin/apps/templates/Built-In", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, templates) = sc.call("GET", "/api/admin/apps/templates", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        templates,
        Some(json!([ { "name": "Built-In", "settings": { "TITLE": "Built In" } } ]))
    );
}

// ---------------------------------------------------------------------------
// Template schema (the base layer of the template editor)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_save_schema_missing_settings() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/apps/templates/schema", Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "'settings' is required.");
}

#[tokio::test]
async fn test_save_schema_not_a_list() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": "nope" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "'settings' must be a list.");
}

#[tokio::test]
async fn test_save_schema_entry_not_object() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [42] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "Each setting must be an object.");
}

#[tokio::test]
async fn test_save_schema_invalid_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [{ "name": "bad name" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "Invalid setting name: 'bad name'.");
}

#[tokio::test]
async fn test_save_schema_duplicate_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [{ "name": "TITLE" }, { "name": "TITLE" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "Duplicate setting name: 'TITLE'.");
}

#[tokio::test]
async fn test_save_schema_invalid_type() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [{ "name": "TITLE", "type": "bogus" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body.unwrap()["detail"],
        "Invalid type 'bogus' for setting 'TITLE'."
    );
}

#[tokio::test]
async fn test_save_schema_replaces_whole_schema() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [{ "name": "ONLY_ONE", "label": "Only One", "type": "text", "default": "x" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    assert_eq!(body, Some(json!({ "ok": true })));
    let (status, schema) = sc.call("GET", "/api/admin/apps/templates/schema", None).await;
    assert_eq!(status, StatusCode::OK);
    let settings_list = schema.as_ref().unwrap()["settings"].as_array().unwrap();
    // The save is a wholesale replacement: every seeded setting is gone.
    assert_eq!(settings_list.len(), 1);
    let entry = &settings_list[0];
    assert_eq!(entry["name"], "ONLY_ONE");
    assert_eq!(entry["label"], "Only One");
    assert_eq!(entry["default"], "x");
    assert!(entry["current"].is_null());
    assert_eq!(entry["builtin"], false);
}

#[tokio::test]
async fn test_save_schema_builtin_default_tracks_yaml() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Built-in baselines are read-only: an admin-supplied default is
    // ignored and the bundled YAML value wins.
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [{ "name": "TITLE", "type": "text", "default": "Wrong" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    let (_status, schema) = sc.call("GET", "/api/admin/apps/templates/schema", None).await;
    let entry = schema
        .as_ref()
        .unwrap()["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "TITLE")
        .unwrap();
    assert_eq!(entry["default"], "Selkies");
    assert_eq!(entry["builtin"], true);
}

#[tokio::test]
async fn test_save_schema_empty_current_becomes_null() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // An empty "current" means "use the image default" and is stored as
    // NULL so the setting is not pushed through to launches.
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [{ "name": "TITLE", "type": "text", "current": "" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    let (_status, schema) = sc.call("GET", "/api/admin/apps/templates/schema", None).await;
    let entry = schema
        .as_ref()
        .unwrap()["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "TITLE")
        .unwrap();
    assert!(entry["current"].is_null());
}

#[tokio::test]
async fn test_save_schema_custom_setting_keeps_admin_default() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [{ "name": "MY_CUSTOM_VAR", "type": "text", "default": "base", "current": "override" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    let (_status, schema) = sc.call("GET", "/api/admin/apps/templates/schema", None).await;
    let entry = schema
        .as_ref()
        .unwrap()["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "MY_CUSTOM_VAR")
        .unwrap();
    assert_eq!(entry["default"], "base");
    assert_eq!(entry["current"], "override");
    assert_eq!(entry["builtin"], false);
}

#[tokio::test]
async fn test_save_schema_select_options_filtered() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/templates/schema",
            Some(&json!({ "settings": [{ "name": "WATERMARK_LOCATION", "type": "select", "options": ["junk", { "value": null }, { "value": "1", "label": "TL" }] }] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    let (_status, schema) = sc.call("GET", "/api/admin/apps/templates/schema", None).await;
    let entry = schema
        .as_ref()
        .unwrap()["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "WATERMARK_LOCATION")
        .unwrap();
    // Non-dict entries and entries without a value are dropped ...
    assert_eq!(entry["options"], json!([{ "value": "1", "label": "TL" }]));
    // ... and the built-in baseline is restored regardless.
    assert_eq!(entry["default"], "-1");
}
