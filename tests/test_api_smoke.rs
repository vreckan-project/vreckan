//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! End-to-end smoke test: login, E2EE handshake, encrypted admin flows.
//!
//! Port of `tests/test_api_smoke`.

mod common;

use common::{build_state, SecureClient, TempDir};
use sea_orm::{EntityTrait};
use serde_json::json;
use vreckan::catalog;
use vreckan::entities;

/// The Firefox store entry (mirrors the conftest `STORE_APP`).
const STORE_APP: &str = r#"{
    "id": "firefox",
    "name": "Firefox",
    "logo": "https://example.invalid/firefox.png",
    "url": "https://example.invalid/firefox",
    "provider": "docker",
    "provider_config": {
        "image": "lscr.io/linuxserver/firefox:latest",
        "port": 3000,
        "nvidia_support": true,
        "dri3_support": true,
        "type": "browser",
        "url_support": true,
        "open_support": false,
        "extensions": [["html", "htm"], "pdf"],
        "autostart": false
    }
}"#;

/// Register a store whose cached catalog contains the Firefox entry
/// (mirrors the conftest `store_with_firefox` fixture).
async fn store_with_firefox(state: &vreckan::state::AppState) {
    let cache_dir = std::path::Path::new(&state.settings.app_store_cache_path);
    std::fs::create_dir_all(cache_dir).unwrap();
    let yaml = format!("apps:\n  - {}\n", STORE_APP);
    std::fs::write(cache_dir.join("Test Store.yml"), &yaml).unwrap();
    // Persist the store so a (re)load keeps it.
    let am = entities::app_store::ActiveModel {
        name: sea_orm::ActiveValue::Set("Test Store".to_string()),
        url: sea_orm::ActiveValue::Set(format!("{}apps.yml", common::DEAD_URL)),
    };
    entities::app_store::Entity::insert(am)
        .exec(&state.db.conn)
        .await
        .unwrap();
    // Mirror the in-memory store list.
    state
        .app_stores
        .write()
        .await
        .push(catalog::AppStore {
            name: "Test Store".to_string(),
            url: format!("{}apps.yml", common::DEAD_URL),
        });
}

#[tokio::test]
async fn test_template_schema_served_to_admin() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call("GET", "/api/admin/apps/templates/schema", None)
        .await;
    assert_eq!(status.as_u16(), 200);
    let data = data.unwrap();
    let names: Vec<&str> = data["settings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(names.contains(&"SELKIES_ENCODER"));
    assert!(names.contains(&"DOCKER_PRIVILEGED"));
}

#[tokio::test]
async fn test_status_install_patch_flow() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    store_with_firefox(&sc.state).await;

    let (status, data) = sc.call("POST", "/api/admin/status", Some(&json!({}))).await;
    assert_eq!(status.as_u16(), 200);
    let data = data.unwrap();
    assert_eq!(data["is_admin"], true);
    assert_eq!(data["username"], "admin");

    let (status, available) = sc
        .call(
            "GET",
            "/api/admin/apps/available?url=http://127.0.0.1:9/apps.yml&store_name=Test%20Store",
            None,
        )
        .await;
    assert_eq!(status.as_u16(), 200);
    let available = available.unwrap();
    assert_eq!(available[0]["id"], "firefox");
    // Nested extension lists from the store YAML are flattened.
    assert_eq!(
        available[0]["provider_config"]["extensions"],
        json!(["html", "htm", "pdf"])
    );

    let mut install_body = available[0].clone();
    if let Some(obj) = install_body.as_object_mut() {
        obj.insert("id".into(), json!("inst-1"));
        obj.insert("source".into(), json!("Test Store"));
        obj.insert("source_app_id".into(), json!("firefox"));
        obj.insert("home_directories".into(), json!(true));
        obj.insert("users".into(), json!(["all"]));
        obj.insert("groups".into(), json!([]));
        obj.insert("app_template".into(), json!("Default"));
    }
    let (status, installed) = sc
        .call("POST", "/api/admin/apps/installed", Some(&install_body))
        .await;
    assert_eq!(status.as_u16(), 201);
    let installed = installed.unwrap();
    assert_eq!(installed["name"], "Firefox");

    // Installing the same ID again is a conflict.
    let (status, _) = sc
        .call("POST", "/api/admin/apps/installed", Some(&install_body))
        .await;
    assert_eq!(status.as_u16(), 409);

    // PUT replaces the whole record (logo emptied to skip icon handling).
    let mut full = installed.clone();
    if let Some(obj) = full.as_object_mut() {
        obj.insert("name".into(), json!("Work Firefox"));
        obj.insert("users".into(), json!(["admin"]));
        obj.insert("logo".into(), json!(""));
    }
    let (status, patched) = sc
        .call("PUT", "/api/admin/apps/installed/inst-1", Some(&full))
        .await;
    assert_eq!(status.as_u16(), 200);
    let patched = patched.unwrap();
    assert_eq!(patched["name"], "Work Firefox");
    assert_eq!(patched["users"], json!(["admin"]));

    // The database row reflects the update.
    let row = entities::installed_app::Entity::find_by_id("inst-1".to_string())
        .one(&sc.state.db.conn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.data["name"], "Work Firefox");

    let (status, apps) = sc.call("POST", "/api/applications", Some(&json!({}))).await;
    assert_eq!(status.as_u16(), 200);
    let apps = apps.unwrap();
    let names: Vec<&str> = apps
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert_eq!(names, vec!["Work Firefox"]);
    // The payload exposes the store-config app type (data only, for the
    // frontend; ANT-228).
    let item = &apps.as_array().unwrap()[0];
    assert_eq!(item["type"], "browser");

    let (status, listing) = sc.call("GET", "/api/admin/apps/installed", None).await;
    assert_eq!(status.as_u16(), 200);
    let listing = listing.unwrap();
    assert!(listing[0]["provider_config"]["image"]
        .as_str()
        .unwrap()
        .ends_with("firefox:latest"));

    let (status, _) = sc.call("DELETE", "/api/admin/apps/installed/inst-1", None).await;
    assert_eq!(status.as_u16(), 204);
    let (status, listing) = sc.call("GET", "/api/admin/apps/installed", None).await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!(listing, Some(json!([])));
}

#[tokio::test]
async fn test_unauthenticated_requests_are_rejected() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    // No credentials at all: the auth dependency rejects the request.
    assert_eq!(
        sc.http
            .post_json("/api/admin/status", &json!({}))
            .await
            .status
            .as_u16(),
        401
    );
    // ...same with a bogus session header.
    let mut headers = std::collections::HashMap::new();
    headers.insert("X-Session-ID".to_string(), "nope".to_string());
    let resp = sc
        .http
        .request(
            "POST",
            "/api/admin/status",
            Some(json!({}).to_string().into_bytes()),
            Some("application/json"),
            &headers,
        )
        .await;
    assert_eq!(resp.status.as_u16(), 401);
    // Unknown paths still 404 through the static catch-all.
    assert_eq!(sc.http.get("/internal/resolve_session/abc").await.status.as_u16(), 404);
}

#[tokio::test]
async fn test_valid_session_but_no_auth_is_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // A valid crypto session without a login cookie still cannot call
    // authenticated endpoints.
    sc.http.auth_cookie = None;
    let mut headers = std::collections::HashMap::new();
    headers.insert("X-Session-ID".to_string(), sc.session_id.clone().unwrap());
    let resp = sc
        .http
        .request(
            "POST",
            "/api/admin/status",
            Some(json!({}).to_string().into_bytes()),
            Some("application/json"),
            &headers,
        )
        .await;
    assert_eq!(resp.status.as_u16(), 401);
}

#[tokio::test]
async fn test_authenticated_without_valid_session_is_blocked() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Authenticated (cookie) but the crypto session is invalid: the
    // encrypted router must not leak the plaintext response body.
    let mut headers = std::collections::HashMap::new();
    headers.insert("X-Session-ID".to_string(), "nope".to_string());
    let resp = sc
        .http
        .request(
            "POST",
            "/api/admin/status",
            Some(json!({}).to_string().into_bytes()),
            Some("application/json"),
            &headers,
        )
        .await;
    assert_eq!(resp.status.as_u16(), 400);
    let detail = resp
        .json()
        .get("detail")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    assert!(detail.contains("secure session"));
}
