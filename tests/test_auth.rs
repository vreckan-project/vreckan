//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Web-login authentication: opaque tokens, cookies, Bearer, lifecycle.
//!
//! Port of `tests/test_auth`.

mod common;

use common::{build_state, SecureClient, TempDir};
use serde_json::json;
use vreckan::user_manager;

const ADMIN: (&str, &str) = ("admin", "admin1234");

#[tokio::test]
async fn test_login_success() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": ADMIN.0, "password": ADMIN.1 }))
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    let body = resp.json();
    assert_eq!(body["username"], "admin");
    assert_eq!(body["is_admin"], true);
    assert_eq!(body["active"], true);
    assert_eq!(body["has_password"], true);
    // The credential is issued both as an HttpOnly cookie and a header.
    let set_cookie = resp.header("set-cookie").unwrap_or_default();
    assert!(set_cookie.to_lowercase().contains("httponly"));
    assert!(resp.header("x-auth-token").is_some());
}

#[tokio::test]
async fn test_login_wrong_password() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": "admin", "password": "nope" }))
        .await;
    assert_eq!(resp.status.as_u16(), 401);
}

#[tokio::test]
async fn test_login_unknown_user() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": "ghost", "password": "whatever" }))
        .await;
    assert_eq!(resp.status.as_u16(), 401);
}

#[tokio::test]
async fn test_me_with_cookie() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    assert_eq!(sc.http.login(ADMIN.0, ADMIN.1).await.status.as_u16(), 200);
    let resp = sc.http.get("/api/auth/me").await;
    assert_eq!(resp.status.as_u16(), 200);
    assert_eq!(resp.json()["username"], "admin");
}

#[tokio::test]
async fn test_me_with_bearer_token() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let login = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": ADMIN.0, "password": ADMIN.1 }))
        .await;
    let token = login.header("x-auth-token").unwrap();
    // Drop the cookie: authenticate via the header only.
    sc.http.auth_cookie = None;
    let mut headers = std::collections::HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    let resp = sc.http.get_with("/api/auth/me", &headers).await;
    assert_eq!(resp.status.as_u16(), 200);
    assert_eq!(resp.json()["username"], "admin");
}

#[tokio::test]
async fn test_me_without_credentials() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    assert_eq!(sc.http.get("/api/auth/me").await.status.as_u16(), 401);
}

#[tokio::test]
async fn test_me_with_garbage_bearer() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let mut headers = std::collections::HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer not-a-issued-token".to_string());
    assert_eq!(sc.http.get_with("/api/auth/me", &headers).await.status.as_u16(), 401);
}

#[tokio::test]
async fn test_logout_revokes_the_token() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    sc.http
        .post_json("/api/auth/login", &json!({ "username": ADMIN.0, "password": ADMIN.1 }))
        .await;
    assert_eq!(sc.http.post_json("/api/auth/logout", &json!({})).await.status.as_u16(), 200);
    assert_eq!(sc.http.get("/api/auth/me").await.status.as_u16(), 401);
}

#[tokio::test]
async fn test_change_password() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": ADMIN.1, "new_password": "freshpass1" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    assert!(user_manager::verify_password(&sc.state.db, "admin", "freshpass1")
        .await
        .unwrap_or(false));
    assert!(!user_manager::verify_password(&sc.state.db, "admin", ADMIN.1)
        .await
        .unwrap_or(false));
}

#[tokio::test]
async fn test_change_password_wrong_old_password() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": "wrong", "new_password": "freshpass1" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 401);
    assert_eq!(resp.json()["detail"], "Current password is incorrect.");
}

#[tokio::test]
async fn test_change_password_short_new_password() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": ADMIN.1, "new_password": "short" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 422);
}

#[tokio::test]
async fn test_change_password_keeps_session_valid() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": ADMIN.1, "new_password": "freshpass1" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    // The opaque token is not password-bound: the live session survives.
    assert_eq!(sc.http.get("/api/auth/me").await.status.as_u16(), 200);
}

#[tokio::test]
async fn test_set_password_requires_admin() {
    // No credentials at all: the auth guard rejects before the admin check.
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/auth/set_password", &json!({ "password": "somepass123" }))
        .await;
    assert_eq!(resp.status.as_u16(), 401);
}

#[tokio::test]
async fn test_set_password_for_other_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    user_manager::create_user(&sc.state.db, "bob", None).await.unwrap();
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/set_password?target=bob",
            &json!({ "password": "bobpass123" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    assert!(user_manager::verify_password(&sc.state.db, "bob", "bobpass123")
        .await
        .unwrap_or(false));
    // bob can now log in with the assigned password.
    assert_eq!(
        sc.http
            .post_json("/api/auth/login", &json!({ "username": "bob", "password": "bobpass123" }))
            .await
            .status
            .as_u16(),
        200
    );
}

#[tokio::test]
async fn test_inactive_user_cannot_login() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    // Deactivate the admin account.
    let user = user_manager::get_user(&sc.state.db, "admin").await.unwrap().unwrap();
    let mut settings = user.settings.unwrap_or_else(|| json!({}));
    settings["active"] = json!(false);
    user_manager::update_user_settings(&sc.state.db, "admin", settings).await.unwrap();
    let resp = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": ADMIN.0, "password": ADMIN.1 }))
        .await;
    assert_eq!(resp.status.as_u16(), 403);
}

#[tokio::test]
async fn test_admin_only_endpoint_rejects_regular_user() {
    // /api/admin/* (unlike /api/admin/status) sits behind verify_admin.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    user_manager::create_user(&sc.state.db, "bob", None).await.unwrap();
    user_manager::set_password(&sc.state.db, "bob", "bobpass123").await.unwrap();
    // Switch the cookie to the regular user's token.
    let resp = sc.http.login("bob", "bobpass123").await;
    assert_eq!(resp.status.as_u16(), 200);
    sc.handshake().await;
    let (status, _) = sc.call("POST", "/api/admin/data", Some(&json!({}))).await;
    assert_eq!(status.as_u16(), 403);
}
