//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Admin endpoints not covered by the other suites: global settings (GPU
//! default), session management, and the installed-apps list / install /
//! pull-latest flows.
//!
//! Ports `tests/test_admin_misc`. Session records are seeded directly into
//! `state.sessions` with the same shape `_launch_common` stores. The
//! `provider_app_id` points at an app that is NOT installed, so the
//! stop-session cleanup path has no containers to talk to (no Docker daemon
//! needed).

mod common;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;

/// A valid install body (InstalledApp) for the firefox store app.
const INSTALL_BODY: &str = r#"{
    "id": "app-1",
    "name": "Firefox",
    "logo": "x",
    "url": "https://example.invalid/firefox",
    "source": "Test Store",
    "source_app_id": "firefox",
    "provider": "docker",
    "home_directories": true,
    "users": ["all"],
    "groups": [],
    "app_template": "Default",
    "provider_config": {
        "image": "lscr.io/linuxserver/firefox:latest",
        "port": 3000,
        "nvidia_support": true,
        "dri3_support": true,
        "type": "browser",
        "url_support": true,
        "open_support": false,
        "extensions": ["html", "htm", "pdf"],
        "autostart": false
    }
}"#;

/// A device path the endpoint will accept. The test host may expose a real DRI
/// device (detected at startup); prefer that, and only seed a fake GPU when
/// the host has none.
async fn gpu_device(sc: &SecureClient) -> String {
    let gpus = sc.state.available_gpus.read().await.clone();
    if let Some(first) = gpus.first() {
        return first.device.clone();
    }
    let fake = vreckan::docker_provider::GpuInfo {
        device: "/dev/dri/renderD128".to_string(),
        driver: "amdgpu".to_string(),
        r#type: Some("dri3".to_string()),
        index: None,
    };
    sc.state.available_gpus.write().await.push(fake);
    "/dev/dri/renderD128".to_string()
}

/// Insert a session record shaped like the one `_launch_common` stores.
async fn seed_session(sc: &SecureClient, username: &str, overrides: serde_json::Value) -> String {
    let mut record = json!({
        "instance_id": "ctr-1",
        "ip": null,
        "port": null,
        "created_at": 1.0,
        "access_token": format!("tok-{}", uuid::Uuid::new_v4()),
        "controller_token": format!("ctrl-{}", uuid::Uuid::new_v4()),
        "provider_app_id": "ghost-app",
        "username": username,
        "app_name": "Test App",
        "app_logo": "x",
        "name": null,
        "host_mount_path": null,
        "shared_files_path": null,
        "launch_context": null,
        "is_collaboration": false,
        "container_registry": {},
    });
    if let (Some(obj), Some(ov)) = (record.as_object_mut(), overrides.as_object()) {
        for (k, v) in ov {
            obj.insert(k.clone(), v.clone());
        }
    }
    let sid = uuid::Uuid::new_v4().to_string();
    sc.state.sessions.write().await.insert(sid.clone(), record);
    sid
}

// ---------------------------------------------------------------------------
// Global settings (GPU default)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_global_settings_fresh() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("GET", "/api/admin/global-settings", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(data.as_ref().unwrap()["global_default_gpu"].is_null());
    // The GPU list mirrors whatever the test host exposes at startup.
    let gpus = sc.state.available_gpus.read().await.clone();
    assert_eq!(data.as_ref().unwrap()["gpus"], serde_json::to_value(&gpus).unwrap());
}

#[tokio::test]
async fn test_global_settings_unknown_gpu_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/admin/global-settings",
            Some(&json!({ "global_default_gpu": "/dev/dri/renderD999" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        data.unwrap()["detail"],
        "Unknown GPU device: '/dev/dri/renderD999'."
    );
}

#[tokio::test]
async fn test_global_settings_set_and_persist() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let device = gpu_device(&sc).await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/admin/global-settings",
            Some(&json!({ "global_default_gpu": device })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!({ "ok": true })));
    let (status, data) = sc.call("GET", "/api/admin/global-settings", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["global_default_gpu"], device);
    // The choice is persisted to the app_settings shadow table.
    let row = vreckan::entities::app_setting::Entity::find_by_id("global_default_gpu".to_string())
        .one(&sc.state.db.conn)
        .await
        .unwrap();
    assert_eq!(row.as_ref().unwrap().value, device);
}

#[tokio::test]
async fn test_global_settings_clear() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let device = gpu_device(&sc).await;
    sc.call(
        "POST",
        "/api/admin/global-settings",
        Some(&json!({ "global_default_gpu": device })),
    )
    .await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/admin/global-settings",
            Some(&json!({ "global_default_gpu": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!({ "ok": true })));
    let (_status, data) = sc.call("GET", "/api/admin/global-settings", None).await;
    assert!(data.as_ref().unwrap()["global_default_gpu"].is_null());
    let count = vreckan::entities::app_setting::Entity::find()
        .filter(vreckan::entities::app_setting::Column::Key.eq("global_default_gpu"))
        .all(&sc.state.db.conn)
        .await
        .unwrap()
        .len();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_global_settings_missing_key_is_noop() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let device = gpu_device(&sc).await;
    sc.call(
        "POST",
        "/api/admin/global-settings",
        Some(&json!({ "global_default_gpu": device })),
    )
    .await;
    let (status, data) = sc
        .call("POST", "/api/admin/global-settings", Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!({ "ok": true })));
    let (_status, data) = sc.call("GET", "/api/admin/global-settings", None).await;
    assert_eq!(data.as_ref().unwrap()["global_default_gpu"], device);
}

// ---------------------------------------------------------------------------
// SSO groups
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sso_groups_fresh() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // No SSO groups are configured in the test environment, and no SSO users
    // exist yet, so all three lists are empty.
    let (status, data) = sc.call("GET", "/api/admin/sso-groups", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["admin_groups"], json!([]));
    assert_eq!(data.as_ref().unwrap()["user_groups"], json!([]));
    assert_eq!(data.as_ref().unwrap()["observed"], json!([]));
}

#[tokio::test]
async fn test_sso_groups_reflects_env() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // The admin/user group lists are read from the environment (exposed as
    // settings attributes), so the endpoint reflects the configured values.
    // The router holds a clone of the state, so rebuild it after mutating
    // settings (preserving the auth cookie).
    sc.state.settings.oidc_admin_groups = "vdi-admins, ops".to_string();
    sc.state.settings.oidc_user_groups = "vdi-users".to_string();
    let cookie = sc.http.auth_cookie.clone();
    sc.http = common::TestClient::new(sc.state.clone());
    sc.http.auth_cookie = cookie;
    let (status, data) = sc.call("GET", "/api/admin/sso-groups", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["admin_groups"], json!(["vdi-admins", "ops"]));
    assert_eq!(data.as_ref().unwrap()["user_groups"], json!(["vdi-users"]));
}

#[tokio::test]
async fn test_sso_groups_observed() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Seed two SSO users directly in the DB.
    for (username, groups) in [
        ("obs-a", json!(["vdi-users", "vdi-admins"])),
        ("obs-b", json!(["vdi-users"])),
    ] {
        let am = vreckan::entities::user::ActiveModel {
            username: sea_orm::ActiveValue::Set(username.to_string()),
            is_admin: sea_orm::ActiveValue::Set(false),
            is_sso: sea_orm::ActiveValue::Set(true),
            settings: sea_orm::ActiveValue::Set(Some(json!({}))),
            password_hash: sea_orm::ActiveValue::Set(None),
            must_change_password: sea_orm::ActiveValue::NotSet,
            sso_groups: sea_orm::ActiveValue::Set(Some(groups)),
            roles: sea_orm::ActiveValue::Set(Some(json!([]))),
            permissions: sea_orm::ActiveValue::Set(Some(json!([]))),
            groups: sea_orm::ActiveValue::Set(Some(json!([]))),
        };
        vreckan::entities::user::Entity::insert(am)
            .exec(&sc.state.db.conn)
            .await
            .unwrap();
    }
    // A non-SSO user's groups are not counted as observed SSO groups.
    let am = vreckan::entities::user::ActiveModel {
        username: sea_orm::ActiveValue::Set("local".to_string()),
        is_admin: sea_orm::ActiveValue::Set(false),
            is_sso: sea_orm::ActiveValue::Set(false),
            settings: sea_orm::ActiveValue::Set(Some(json!({}))),
            password_hash: sea_orm::ActiveValue::Set(Some("x".to_string())),
            must_change_password: sea_orm::ActiveValue::NotSet,
            sso_groups: sea_orm::ActiveValue::Set(Some(json!(["should-not-appear"]))),
        roles: sea_orm::ActiveValue::Set(Some(json!([]))),
        permissions: sea_orm::ActiveValue::Set(Some(json!([]))),
        groups: sea_orm::ActiveValue::Set(Some(json!([]))),
    };
    vreckan::entities::user::Entity::insert(am)
        .exec(&sc.state.db.conn)
        .await
        .unwrap();
    let (status, data) = sc.call("GET", "/api/admin/sso-groups", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["observed"], json!(["vdi-admins", "vdi-users"]));
}

// ---------------------------------------------------------------------------
// Admin session management
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_admin_sessions_empty() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("GET", "/api/admin/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!([])));
}

#[tokio::test]
async fn test_admin_sessions_grouped_and_sorted() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid_old = seed_session(&sc, "bob", json!({ "created_at": 1000.0, "name": "Old" })).await;
    let sid_new = seed_session(&sc, "bob", json!({ "created_at": 2000.0, "name": "New" })).await;
    seed_session(&sc, "admin", json!({ "created_at": 1500.0 })).await;
    let (status, data) = sc.call("GET", "/api/admin/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = data.as_ref().unwrap().as_array().unwrap();
    // Users sorted by name; sessions within a user newest-first.
    let usernames: Vec<&str> = arr.iter().map(|u| u["username"].as_str().unwrap()).collect();
    assert_eq!(usernames, vec!["admin", "bob"]);
    let bob = &arr[1];
    let sids: Vec<&str> = bob["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(sids, vec![sid_new.as_str(), sid_old.as_str()]);
    let names: Vec<&str> = bob["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["New", "Old"]);
    // session_url carries the per-session access token.
    let tok = sc
        .state
        .sessions
        .read()
        .await
        .get(&sid_new)
        .unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        bob["sessions"][0]["session_url"],
        format!("/api/apps/session/{sid_new}/?access_token={tok}")
    );
}

#[tokio::test]
async fn test_admin_delete_session_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("DELETE", "/api/admin/sessions/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Session not found.");
}

#[tokio::test]
async fn test_admin_delete_session() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(&sc, "admin", json!({})).await;
    let (status, _) = sc.call("DELETE", &format!("/api/admin/sessions/{sid}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!sc.state.sessions.read().await.contains_key(&sid));
}

// ---------------------------------------------------------------------------
// Installed apps: list / install / pull-latest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_installed_apps_empty() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("GET", "/api/admin/apps/installed", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!([])));
}

#[tokio::test]
async fn test_install_app_success() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    // No-op the background pull so the spawned task doesn't do a real docker pull.
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&serde_json::from_str(INSTALL_BODY).unwrap()),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(data.as_ref().unwrap()["id"], "app-1");
    assert!(sc.state.installed_apps.read().await.contains_key("app-1"));
    let (status, listing) = sc.call("GET", "/api/admin/apps/installed", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = listing.as_ref().unwrap().as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Firefox");
    // The image pull is queued (the pull task itself is a no-op in tests).
    assert_eq!(arr[0]["pull_status"], "queued");
    assert!(arr[0]["image_sha"].is_null());
}

#[tokio::test]
async fn test_install_app_duplicate() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    // No-op the background pull so the spawned task doesn't do a real docker pull.
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&serde_json::from_str(INSTALL_BODY).unwrap()),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, data) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&serde_json::from_str(INSTALL_BODY).unwrap()),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(data.unwrap()["detail"], "App with this ID already exists.");
}

#[tokio::test]
async fn test_pull_latest_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call("POST", "/api/admin/apps/installed/ghost/pull_latest", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Installed app not found.");
}

#[tokio::test]
async fn test_pull_latest_already_queued() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    // No-op the background pull so the spawned task doesn't do a real docker pull.
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    sc.call(
        "POST",
        "/api/admin/apps/installed",
        Some(&serde_json::from_str(INSTALL_BODY).unwrap()),
    )
    .await;
    let (status, data) = sc
        .call("POST", "/api/admin/apps/installed/app-1/pull_latest", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!({ "status": "pulling", "new_sha": null })));
}

#[tokio::test]
async fn test_pull_latest_idle_app() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    // No-op the background pull so the spawned task doesn't do a real docker pull.
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let image = "registry.example.invalid/other:latest";
    let mut body: serde_json::Value = serde_json::from_str(INSTALL_BODY).unwrap();
    body["id"] = json!("app-2");
    body["provider_config"]["image"] = json!(image);
    let app: vreckan::catalog::InstalledApp = serde_json::from_value(body).unwrap();
    sc.state.installed_apps.write().await.insert(app.id.clone(), app);
    assert!(!sc.state.pull_status.read().await.contains_key(image));
    let (status, data) = sc
        .call("POST", "/api/admin/apps/installed/app-2/pull_latest", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!({ "status": "pulling", "new_sha": null })));
    assert_eq!(
        sc.state.pull_status.read().await.get(image).map(|s| s.as_str()),
        Some("queued")
    );
}

// ---------------------------------------------------------------------------
// Swallowed DB writes: a failing configuration database must surface as a
// 500, not `ok: true` (mirrors the Python handlers, where a DB error raises
// out of the endpoint).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_global_settings_db_write_failure_returns_500() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let device = gpu_device(&sc).await;
    // Simulate a configuration-database outage for the write path.
    sc.state.db.fail_writes.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, data) = sc
        .call(
            "POST",
            "/api/admin/global-settings",
            Some(&json!({ "global_default_gpu": device })),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{:?}", data);
    assert!(data.as_ref().unwrap().get("detail").is_some(), "{:?}", data);
    assert_ne!(data.as_ref().unwrap().get("ok"), Some(&json!(true)));
    // The in-memory value must not have moved either (no partial success).
    assert_eq!(sc.state.global_default_gpu.read().await.as_deref(), None);
}

#[tokio::test]
async fn test_sso_save_db_write_failure_returns_500() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    sc.state.db.fail_writes.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, data) = sc
        .call(
            "PUT",
            "/api/admin/sso",
            Some(&json!({
                "oidc_enabled": true,
                "oidc_issuer": "https://idp.example/o/app/",
                "oidc_client_id": "cid",
                "oidc_client_secret": "",
                "oidc_redirect_uri": "",
                "oidc_scopes": "openid email password",
                "oidc_allow_signup": true,
                "oidc_state_ttl_seconds": 600,
                "oidc_admin_groups": ["vdi-admins"],
                "oidc_user_groups": ["vdi-users"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{:?}", data);
    assert!(data.as_ref().unwrap().get("detail").is_some(), "{:?}", data);
    assert_ne!(data.as_ref().unwrap().get("ok"), Some(&json!(true)));
}

// ---------------------------------------------------------------------------
// Auto-update settings (global enable + interval)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auto_update_settings_save() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // An empty body changes nothing; the response reflects the effective
    // values (the test state forces auto_update_apps off so no background
    // job runs).
    let (status, data) = sc.call("PUT", "/api/admin/apps/auto_update", Some(&json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
    assert_eq!(data.as_ref().unwrap()["enabled"], false);
    assert_eq!(data.as_ref().unwrap()["interval_seconds"], 3600);
    // Save custom values; they persist through the `app_settings` table and
    // round-trip as the resolved values.
    let (status, data) = sc
        .call(
            "PUT",
            "/api/admin/apps/auto_update",
            Some(&json!({ "enabled": true, "interval_seconds": 21600 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
    assert_eq!(data.as_ref().unwrap()["enabled"], true);
    assert_eq!(data.as_ref().unwrap()["interval_seconds"], 21600);
    assert_eq!(
        sc.state.db.get_app_setting("VRECKAN_AUTO_UPDATE_APPS").await.unwrap().as_deref(),
        Some("true")
    );
    assert_eq!(
        sc.state
            .db
            .get_app_setting("VRECKAN_AUTO_UPDATE_INTERVAL_SECONDS")
            .await
            .unwrap()
            .as_deref(),
        Some("21600")
    );
    // Partial update: only the interval changes.
    let (status, data) = sc
        .call(
            "PUT",
            "/api/admin/apps/auto_update",
            Some(&json!({ "interval_seconds": 86400 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
    assert_eq!(data.as_ref().unwrap()["enabled"], true);
    assert_eq!(data.as_ref().unwrap()["interval_seconds"], 86400);
}
