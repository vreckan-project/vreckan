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
async fn test_my_sessions_lists_all_with_annotations() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let mine = seed_session(&sc, "admin", json!({})).await;
    let other = seed_session(&sc, "ghost", json!({})).await; // someone else's session
    let (status, data) = sc.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = data.as_ref().unwrap().as_array().unwrap();
    // ANT-440: the list includes other users' running sessions (not just the
    // caller's own), each annotated for the Join affordance.
    assert_eq!(arr.len(), 2);
    // The sessions map iterates in arbitrary order, so locate each entry by
    // its id rather than relying on position.
    let mine_info = arr.iter().find(|s| s["session_id"] == mine).unwrap();
    let other_info = arr.iter().find(|s| s["session_id"] == other).unwrap();

    // The caller's own session is flagged as theirs and exposes its
    // access-token URL (non-collaboration sessions use the access token),
    // with the same value mirrored under `token` for the WebRTC client
    // (ANT-449).
    assert_eq!(mine_info["is_mine"], true);
    assert_eq!(mine_info["username"], "admin");
    assert_eq!(mine_info["app_name"], "Test App");
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
        mine_info["session_url"],
        format!("/api/apps/session/{mine}/?access_token={tok}&token={tok}")
    );

    // Another user's session is flagged not-mine, names its owner, and has no
    // direct-open URL (the caller enters it through a Join-minted token).
    assert_eq!(other_info["is_mine"], false);
    assert_eq!(other_info["username"], "ghost");
    assert_eq!(other_info["session_url"], json!(null));

    // The seeded sessions point at an uninstalled app, so neither is joinable.
    assert_eq!(mine_info["joinable"], false);
    assert_eq!(other_info["joinable"], false);
}

// ---------------------------------------------------------------------------
// Self-serve join (ANT-440)
// ---------------------------------------------------------------------------

/// Insert an installed app (all users may access it by default) into the
/// in-memory table. Returns the app id.
async fn seed_app(sc: &SecureClient, id: &str, users: &[&str]) -> String {
    let data = json!({
        "id": id,
        "name": "Join App",
        "logo": "x",
        "url": "x",
        "source": "Test Store",
        "source_app_id": id,
        "provider": "docker",
        "home_directories": true,
        "users": users,
        "groups": [],
        "app_template": "Default",
        "provider_config": { "image": "img:latest", "port": 3000 },
    });
    let app: vreckan::catalog::InstalledApp = serde_json::from_value(data).unwrap();
    let app_id = app.id.clone();
    sc.state
        .installed_apps
        .write()
        .await
        .insert(app_id.clone(), app);
    app_id
}

#[tokio::test]
async fn test_join_mints_participant_token_and_is_idempotent() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // An installed app everyone can use, and a session for it owned by
    // someone else (the joiner joins the owner's running session).
    let app_id = seed_app(&sc, "join-app", &["all"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;

    // The session list now surfaces the other user's session as joinable.
    let (status, data) = sc.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = data.as_ref().unwrap().as_array().unwrap();
    let info = arr.iter().find(|s| s["session_id"] == other).unwrap();
    assert_eq!(info["is_mine"], false);
    assert_eq!(info["username"], "ghost");
    assert_eq!(info["joinable"], true);
    assert_eq!(info["session_url"], json!(null));

    // Join: admin mints their own participant token into ghost's session.
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "join should succeed: {data:?}");
    let url = data.as_ref().unwrap()["session_url"].as_str().unwrap().to_string();
    let join_mode = data.as_ref().unwrap()["join_mode"].as_str().unwrap();
    // The test container is unreachable, so the session stays legacy.
    assert_eq!(join_mode, "legacy");
    assert!(url.starts_with(&format!("/api/apps/session/{other}/?token=")), "{url}");
    let minted = url.rsplit_once("token=").unwrap().1.to_string();

    // The token is recorded in the session's viewers list as a participant.
    let viewers = sc
        .state
        .sessions
        .read()
        .await
        .get(&other)
        .unwrap()
        .get("viewers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let entry = viewers
        .iter()
        .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(minted.as_str()))
        .expect("minted token in viewers");
    assert_eq!(entry["username"], "admin");
    assert_eq!(entry["permission"], "participant");

    // Idempotent: re-joining returns the same token, not a second one.
    let (status, data2) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let url2 = data2.as_ref().unwrap()["session_url"].as_str().unwrap().to_string();
    assert_eq!(url2, url);
    let viewers_after = sc
        .state
        .sessions
        .read()
        .await
        .get(&other)
        .unwrap()
        .get("viewers")
        .and_then(|v| v.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    assert_eq!(viewers_after, 1, "re-join must not mint a second token");
}

#[tokio::test]
async fn test_join_room_conflict() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let app_id = seed_app(&sc, "join-app", &["all"]).await;
    let room = seed_session(
        &sc,
        "ghost",
        json!({ "provider_app_id": app_id, "is_collaboration": true }),
    )
    .await;
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{room}/join"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "rooms use their own invite flow: {data:?}");
}

#[tokio::test]
async fn test_join_no_app_access_forbidden() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // An app only "ghost" can use; admin joins a session for it.
    let app_id = seed_app(&sc, "ghost-only-app", &["ghost"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{data:?}");
}

#[tokio::test]
async fn test_join_unknown_session_not_found() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _data) = sc
        .call("POST", "/api/sessions/does-not-exist/join", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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
