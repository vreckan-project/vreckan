//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! User-facing session endpoints (`/api/sessions`): list my sessions, poll
//! readiness, stop, and send files — the self-service counterpart of the admin
//! session views.
//!
//! Ports `tests/test_user_sessions`. Session records are seeded directly
//! into `state.sessions` with the same shape `_launch_common` stores. The
//! `provider_app_id` points at an app that is NOT installed, so the
//! stop-session cleanup path has no containers to talk to (no Docker daemon
//! needed). "Other user" cases use the non-existent username "ghost".

mod common;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;

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
// List my sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_my_sessions_requires_auth() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    assert_eq!(sc.http.get("/api/sessions").await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_my_sessions_empty() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!([])));
}

#[tokio::test]
async fn test_my_sessions_only_own() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let mine = seed_session(&sc, "admin", json!({})).await;
    seed_session(&sc, "ghost", json!({})).await; // someone else's session
    let (status, data) = sc.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = data.as_ref().unwrap().as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["session_id"], mine);
    assert_eq!(arr[0]["app_name"], "Test App");
    // Non-collaboration sessions expose the access-token URL.
    let tok = sc
        .state
        .sessions
        .read()
        .await
        .get(&mine)
        .unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        arr[0]["session_url"],
        format!("/api/apps/session/{mine}/?access_token={tok}")
    );
}

// ---------------------------------------------------------------------------
// Session status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_status_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("GET", "/api/sessions/nope/status", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Session not found or permission denied.");
}

#[tokio::test]
async fn test_session_status_other_user_denied() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let other = seed_session(&sc, "ghost", json!({})).await;
    let (status, data) = sc
        .call("GET", &format!("/api/sessions/{other}/status"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Session not found or permission denied.");
}

#[tokio::test]
async fn test_session_status_no_upstream() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(&sc, "admin", json!({})).await; // ip=None, port=None
    let (status, data) = sc
        .call("GET", &format!("/api/sessions/{sid}/status"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        data,
        Some(json!({ "ready": false, "detail": "No upstream address recorded." }))
    );
}

#[tokio::test]
async fn test_session_status_unreachable() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Port 9 (discard) is not listening.
    let sid = seed_session(&sc, "admin", json!({ "ip": "127.0.0.1", "port": 9 })).await;
    let (status, data) = sc
        .call("GET", &format!("/api/sessions/{sid}/status"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["ready"], false);
    assert!(data.as_ref().unwrap()["detail"]
        .as_str()
        .unwrap()
        .starts_with("unreachable ("));
}

// ---------------------------------------------------------------------------
// Stop a session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stop_session_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("DELETE", "/api/sessions/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Session not found or permission denied.");
}

#[tokio::test]
async fn test_stop_session_other_user_denied() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let other = seed_session(&sc, "ghost", json!({})).await;
    let (status, data) = sc.call("DELETE", &format!("/api/sessions/{other}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Session not found or permission denied.");
    assert!(sc.state.sessions.read().await.contains_key(&other)); // untouched
}

#[tokio::test]
async fn test_stop_my_session() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(&sc, "admin", json!({})).await;
    let (status, _) = sc.call("DELETE", &format!("/api/sessions/{sid}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!sc.state.sessions.read().await.contains_key(&sid));
}

// ---------------------------------------------------------------------------
// Send a file to a session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_send_file_malformed_body() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(&sc, "admin", json!({})).await;
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{sid}/send_file"), Some(&json!({ "filename": "a.txt" })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().contains("Invalid request body"));
}

#[tokio::test]
async fn test_send_file_unknown_session() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/sessions/nope/send_file",
            Some(&json!({ "filename": "a.txt", "upload_id": "u-1", "total_chunks": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Session not found or permission denied.");
}

#[tokio::test]
async fn test_send_file_no_mount() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(&sc, "admin", json!({})).await; // host_mount_path=None
    let (status, data) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/send_file"),
            Some(&json!({ "filename": "a.txt", "upload_id": "u-1", "total_chunks": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        data.unwrap()["detail"],
        "Cannot send files to this session as it has no mounted storage."
    );
}
