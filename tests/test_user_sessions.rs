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

#[tokio::test]
async fn test_my_sessions_nulls_launch_context_on_non_joinable_others() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // An app everyone can use (admin can join ghost's session for it) and an
    // app only ghost can use (admin cannot join a session for it).
    let shared = seed_app(&sc, "shared-app", &["all"]).await;
    let ghost_only = seed_app(&sc, "ghost-only-app", &["ghost"]).await;
    // A file launch reveals the launched file name in `launch_context`.
    let launch = json!({ "type": "file", "value": "secret-notes.txt" });

    // (1) admin's own session for the shared app.
    let own = seed_session(
        &sc,
        "admin",
        json!({
            "provider_app_id": shared,
            "launch_context": launch,
            "name": "My notes",
        }),
    )
    .await;
    // (2) ghost's session for the shared app — admin can join it.
    let joinable_other = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": shared,
            "launch_context": launch,
            "name": "Ghost shared",
        }),
    )
    .await;
    // (3) ghost's session for the ghost-only app — admin cannot join it.
    let non_joinable_other = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": ghost_only,
            "launch_context": launch,
            "name": "Ghost secret",
        }),
    )
    .await;

    let (status, data) = sc.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = data.as_ref().unwrap().as_array().unwrap();
    let own_info = arr.iter().find(|s| s["session_id"] == own).unwrap();
    let joinable_info = arr.iter().find(|s| s["session_id"] == joinable_other).unwrap();
    let non_joinable_info = arr.iter().find(|s| s["session_id"] == non_joinable_other).unwrap();

    // (1) Own session: launch_context and name stay intact.
    assert_eq!(own_info["is_mine"], true);
    assert_eq!(own_info["launch_context"], launch);
    assert_eq!(own_info["name"], "My notes");

    // (2) Joinable other session: the caller can open the app, so the context
    // is not sensitive — keep it.
    assert_eq!(joinable_info["is_mine"], false);
    assert_eq!(joinable_info["joinable"], true);
    assert_eq!(joinable_info["launch_context"], launch);
    assert_eq!(joinable_info["name"], "Ghost shared");

    // (3) Non-joinable other session: the row is hidden in the UI, so the
    // launch_context (and custom name) that would reveal the owner's launch
    // are nulled out.
    assert_eq!(non_joinable_info["is_mine"], false);
    assert_eq!(non_joinable_info["joinable"], false);
    assert_eq!(non_joinable_info["launch_context"], json!(null));
    assert_eq!(non_joinable_info["name"], json!(null));
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
    // The client sends an E2EE-encrypted empty body (`body: {}` in
    // secureFetch); the F4 gate (ANT-469) requires it.
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "join should succeed: {data:?}");
    let url = data.as_ref().unwrap()["session_url"].as_str().unwrap().to_string();
    let join_mode = data.as_ref().unwrap()["join_mode"].as_str().unwrap();
    // The test container is unreachable, so the session stays legacy.
    assert_eq!(join_mode, "legacy");
    // ANT-466 F1: the response is the token-free hop URL (the hop hands the
    // token over via the `collab_token` cookie), not the token-bearing
    // session URL.
    assert_eq!(url, format!("/join/{other}/admin"), "{url}");

    // The token is recorded in the session's viewers list as a participant.
    let minted = sc
        .state
        .sessions
        .read()
        .await
        .get(&other)
        .unwrap()
        .get("viewers")
        .and_then(|v| v.as_array())
        .and_then(|viewers| {
            viewers
                .iter()
                .find(|v| {
                    v.get("username").and_then(|u| u.as_str()) == Some("admin")
                        && v.get("permission").and_then(|p| p.as_str()) == Some("participant")
                })
                .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(|t| t.to_string()))
        })
        .expect("admin participant entry in viewers");

    // Idempotent: re-joining returns the same token, not a second one.
    let (status, data2) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
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

    // ANT-466 F1: the hop the join URL points at is a 303 with
    // `Cache-Control: private, no-store`, redirects to the session URL with
    // the token stripped, and hands the (re-used) participant token over
    // through the path-scoped `collab_token` cookie.
    let hop = sc.http.get(&url).await;
    assert_eq!(hop.status, StatusCode::SEE_OTHER, "body: {:?}", hop.body);
    assert_eq!(
        hop.header("cache-control"),
        Some("private, no-store".to_string()),
        "the hop must carry Cache-Control: private, no-store: {:?}",
        hop.headers
    );
    assert_eq!(
        hop.header("location"),
        Some(format!("/api/apps/session/{other}/")),
        "the redirect must be the token-free session URL: {:?}",
        hop.headers
    );
    let cookie = hop.header("set-cookie").unwrap_or_default();
    assert!(
        cookie.starts_with(&format!("collab_token_{other}={minted};")),
        "the hop must hand the participant token over by cookie: {cookie}"
    );
}

/// A throwaway HTTP upstream that answers 200 to every request — stands in
/// for an app container that is reachable (and, for a selkies image, accepts
/// the pushed token table).
async fn spawn_http_upstream() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let body = b"<html>upstream</html>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    std::str::from_utf8(body).unwrap()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// ANT-466 F1: the cookie the join hop sets is, by itself, enough to admit
/// the joiner at the token-free session URL — and nothing is admitted
/// without it.
#[tokio::test]
async fn test_join_hop_cookie_admits_joiner() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (ip, port) = upstream.rsplit_once(':').unwrap();
    let app_id = seed_app(&sc, "join-app", &["all"]).await;
    let other = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": app_id,
            "ip": ip,
            "port": port.parse::<i64>().unwrap(),
        }),
    )
    .await;

    // Join, then follow the hop exactly the way the browser does.
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "join should succeed: {data:?}");
    let hop = sc
        .http
        .get(data.as_ref().unwrap()["session_url"].as_str().unwrap())
        .await;
    assert_eq!(hop.status, StatusCode::SEE_OTHER, "body: {:?}", hop.body);
    let cookie = hop.header("set-cookie").unwrap_or_default();
    let cookie_value = cookie.split(';').next().unwrap_or("").to_string();

    // Cookie handoff: the redirect target, carrying the cookie, is admitted.
    let admitted = sc
        .http
        .get_with(
            &format!("/api/apps/session/{other}/"),
            &std::collections::HashMap::from([("cookie".to_string(), cookie_value)]),
        )
        .await;
    assert_eq!(
        admitted.status,
        StatusCode::OK,
        "the cookie must admit the joiner: {:?}",
        admitted.body
    );

    // No collab cookie: the same URL is refused.
    let denied = sc.http.get(&format!("/api/apps/session/{other}/")).await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "body: {:?}",
        denied.body
    );
}

/// ANT-466 F1: the hop's dead-link states mirror the share entry — 404 when
/// the session is gone, 410 when the caller has no participant entry.
#[tokio::test]
async fn test_join_hop_dead_links() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let app_id = seed_app(&sc, "join-app", &["all"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;

    // Unknown session: 404.
    let unknown = sc.http.get("/join/does-not-exist/admin").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    // A session the caller has no participant entry in: 410.
    let ended = sc.http.get(&format!("/join/{other}/admin")).await;
    assert_eq!(ended.status, StatusCode::GONE);
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
        .call("POST", &format!("/api/sessions/{room}/join"), Some(&json!({})))
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
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
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
        .call("POST", "/api/sessions/does-not-exist/join", Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// F4 (ANT-444 / ANT-469): the join POST enforces the E2EE body (CSRF gate)
// ---------------------------------------------------------------------------
//
// The web client authenticates with the HttpOnly SameSite=Lax login cookie
// and the app has no CORS layer, so a cross-site top-level form POST from a
// logged-in victim reaches the join route fully authenticated. A form cannot
// set the X-Session-ID header or carry an AES-256-GCM body, so the gate
// rejects it (400) before any participant token is minted.

/// Assert the joiner (the test's logged-in user, "admin") holds no
/// participant token in the session — i.e. the rejected request minted
/// nothing.
async fn assert_no_joiner_viewers(sc: &SecureClient, sid: &str) {
    let viewers = sc
        .state
        .sessions
        .read()
        .await
        .get(sid)
        .and_then(|s| s.get("viewers"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        viewers
            .iter()
            .all(|v| v.get("username").and_then(|u| u.as_str()) != Some("admin")),
        "no participant token may be minted: {viewers:?}"
    );
}

#[tokio::test]
async fn test_join_form_post_without_e2ee_body_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    // The victim is logged in (the SameSite=Lax auth cookie rides along on a
    // top-level form POST) but has no crypto session: a form cannot carry
    // the X-Session-ID header.
    sc.http.login("admin", "admin1234").await;
    let app_id = seed_app(&sc, "join-app", &["all"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;

    let resp = sc
        .http
        .post_form(&format!("/api/sessions/{other}/join"), "x")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "cross-site form POST must be rejected 4xx: {:?}",
        resp.body
    );
    assert_no_joiner_viewers(&sc, &other).await;
}

#[tokio::test]
async fn test_join_xhr_without_e2ee_body_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    let app_id = seed_app(&sc, "join-app", &["all"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;

    // Cross-site XHR: the auth cookie rides along (e.g. where Lax is relaxed
    // by ITP-style heuristics), the body is plain JSON, and no X-Session-ID.
    let resp = sc
        .http
        .post_json(&format!("/api/sessions/{other}/join"), &json!({}))
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "cross-site XHR must be rejected 4xx: {:?}",
        resp.body
    );
    assert_no_joiner_viewers(&sc, &other).await;
}

#[tokio::test]
async fn test_join_plaintext_body_with_crypto_session_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let app_id = seed_app(&sc, "join-app", &["all"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;

    // A crypto session exists (X-Session-ID set), but the body is not the
    // AES-256-GCM envelope: decryption must fail and the request be rejected.
    let mut headers = std::collections::HashMap::new();
    headers.insert("x-session-id".to_string(), sc.session_id.clone().unwrap());
    let resp = sc
        .http
        .request(
            "POST",
            &format!("/api/sessions/{other}/join"),
            Some("{}".as_bytes().to_vec()),
            Some("application/json"),
            &headers,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "non-encrypted body must be rejected 4xx: {:?}",
        resp.body
    );
    assert_no_joiner_viewers(&sc, &other).await;
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
