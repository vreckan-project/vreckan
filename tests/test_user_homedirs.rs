//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! User-facing home-directory endpoints (/api/homedirs): the self-service
//! counterpart of the admin people/homedirs API.
//!
//! Port of `tests/test_user_homedirs`.

mod common;

use common::{build_state, SecureClient, TempDir};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel};
use serde_json::json;

#[tokio::test]
async fn test_my_homedirs_requires_auth() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    // No login, no session: the auth guard rejects before anything else.
    assert_eq!(sc.http.get("/api/homedirs").await.status.as_u16(), 401);
}

#[tokio::test]
async fn test_my_homedirs_empty() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("GET", "/api/homedirs", None).await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!(data, Some(json!({ "home_dirs": [] })));
}

#[tokio::test]
async fn test_create_my_home_dir() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call("POST", "/api/homedirs", Some(&json!({ "home_name": "work" })))
        .await;
    assert_eq!(status.as_u16(), 201);
    assert_eq!(data, Some(json!({ "status": "success", "home_name": "work" })));
    let (_status, data) = sc.call("GET", "/api/homedirs", None).await;
    assert_eq!(data, Some(json!({ "home_dirs": ["work"] })));
    // The skeleton directory exists on disk under the user's storage.
    let path = std::path::Path::new(&sc.state.settings.storage_path).join("admin").join("work");
    assert!(path.is_dir());
}

#[tokio::test]
async fn test_create_my_home_dir_duplicate() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    sc.call("POST", "/api/homedirs", Some(&json!({ "home_name": "work" }))).await;
    let (status, data) = sc
        .call("POST", "/api/homedirs", Some(&json!({ "home_name": "work" })))
        .await;
    assert_eq!(status.as_u16(), 409);
    assert_eq!(
        data,
        Some(json!({ "detail": "Home directory 'work' already exists for user 'admin'." }))
    );
}

#[tokio::test]
async fn test_create_my_home_dir_invalid_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call("POST", "/api/homedirs", Some(&json!({ "home_name": "bad name!" })))
        .await;
    assert_eq!(status.as_u16(), 422);
    let detail = data
        .and_then(|d| d.get("detail").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(detail.contains("Invalid request body"));
}

#[tokio::test]
async fn test_homedirs_disabled_when_persistent_storage_off() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Capabilities are permission-driven: strip the admin's roles and direct
    // permissions so user.storage (persistent storage) is lost. Effective
    // permissions re-resolve on every request, so the change takes effect
    // immediately. (The Python test mutates the in-memory USER_DATA directly;
    // here we update the DB row directly, bypassing set_user_access's
    // "bootstrap account must keep the admin role" guard.)
    let model = vreckan::entities::user::Entity::find_by_id("admin".to_string())
        .one(&sc.state.db.conn)
        .await
        .unwrap()
        .unwrap();
    let mut am = model.into_active_model();
    am.roles = sea_orm::ActiveValue::Set(Some(serde_json::json!([])));
    am.permissions = sea_orm::ActiveValue::Set(Some(serde_json::json!([])));
    am.update(&sc.state.db.conn).await.unwrap();
    let (status, data) = sc.call("GET", "/api/homedirs", None).await;
    assert_eq!(status.as_u16(), 403);
    assert_eq!(
        data,
        Some(json!({ "detail": "Persistent storage is disabled for this account." }))
    );
}

#[tokio::test]
async fn test_delete_my_home_dir() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    sc.call("POST", "/api/homedirs", Some(&json!({ "home_name": "work" }))).await;
    let path = std::path::Path::new(&sc.state.settings.storage_path).join("admin").join("work");
    assert!(path.is_dir());
    let (status, _) = sc.call("DELETE", "/api/homedirs/work", None).await;
    assert_eq!(status.as_u16(), 204);
    assert!(!path.exists());
    let (_status, data) = sc.call("GET", "/api/homedirs", None).await;
    assert_eq!(data, Some(json!({ "home_dirs": [] })));
}

#[tokio::test]
async fn test_delete_my_home_dir_missing() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("DELETE", "/api/homedirs/nope", None).await;
    assert_eq!(status.as_u16(), 404);
    assert_eq!(
        data,
        Some(json!({ "detail": "Home directory 'nope' not found for user 'admin'." }))
    );
}
