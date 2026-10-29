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
use vreckan::user_manager;

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

/// Create a real (non-bootstrap) user and return a logged-in, E2EE-handshaked
/// client for them — the owner of a session needs a client to drive the
/// owner-only share API.
async fn second_user(sc: &SecureClient, username: &str, password: &str) -> SecureClient {
    user_manager::create_user(&sc.state.db, username, None).await.unwrap();
    user_manager::set_password(&sc.state.db, username, password).await.unwrap();
    let mut other = SecureClient::new(sc.state.clone());
    other.http.login(username, password).await;
    other.handshake().await;
    other
}

/// The owner creates a share on their session with the given body (including
/// the optional named `username` invite); returns the share id.
async fn create_share(sc: &SecureClient, sid: &str, body: &serde_json::Value) -> String {
    let (status, resp) = sc.call("POST", &format!("/api/sessions/{sid}/shares"), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "create share: {resp:?}");
    resp.unwrap()
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string()
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

/// ANT-472 (with F3 / ANT-468): apps-page visibility of another user's
/// session — and the `launch_context`/`name` that would reveal the owner's
/// launch — follows the owner's express grant, not the caller's app access.
#[tokio::test]
async fn test_my_sessions_visibility_matrix_under_owner_grant() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // A real "ghost" user who owns the other-user sessions and drives their
    // share API.
    let ghost = second_user(&sc, "ghost", "ghostpass123").await;
    // An app everyone can use (admin has app access to it) and an app only
    // ghost can use (admin has none) — the grant must override the latter.
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
    // (2) ghost's session for the shared app, no grant: admin HAS app access
    // but the owner granted none — this is the v0.8.4 self-serve behavior the
    // gate closes.
    let no_grant = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": shared,
            "launch_context": launch,
            "name": "No grant",
        }),
    )
    .await;
    // (3) ghost's session for the ghost-only app with a live named invite for
    // admin: the grant lights it up even though admin has no app access.
    let invited = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": ghost_only,
            "launch_context": launch,
            "name": "Ghost invited",
        }),
    )
    .await;
    let _invite_id = create_share(
        &ghost,
        &invited,
        &json!({ "role": "full", "username": "admin" }),
    )
    .await;
    // (4) Same but the invite has expired: no grant anymore.
    let expired = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": ghost_only,
            "launch_context": launch,
            "name": "Ghost expired",
        }),
    )
    .await;
    let expired_id = create_share(
        &ghost,
        &expired,
        &json!({ "role": "full", "username": "admin", "expiry_hours": 1 }),
    )
    .await;
    {
        let mut shares = sc.state.session_shares.write().await;
        if let Some(meta) = shares.get_mut(&expired_id) {
            meta.expiry_timestamp = Some(1.0);
        }
    }
    // (5) Same but the invite has been revoked: no grant anymore.
    let revoked = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": ghost_only,
            "launch_context": launch,
            "name": "Ghost revoked",
        }),
    )
    .await;
    let revoked_id = create_share(&ghost, &revoked, &json!({ "role": "full", "username": "admin" }))
        .await;
    let (status, _) = ghost
        .call("DELETE", &format!("/api/sessions/{revoked}/shares/{revoked_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "revoke");
    // (6) ghost's session for the shared app with a persisted participant
    // entry for admin and no live link (the grandfathered v0.8.4 joiner).
    let legacy = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": shared,
            "launch_context": launch,
            "name": "Ghost legacy",
            "viewers": [{
                "token": "legacy-tok-1",
                "slot": null,
                "mk_control": false,
                "username": "admin",
                "permission": "participant",
            }],
        }),
    )
    .await;

    let (status, data) = sc.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = data.as_ref().unwrap().as_array().unwrap();
    let info = |id: &str| arr.iter().find(|s| s["session_id"] == id).unwrap();
    let own_info = info(&own);
    let no_grant_info = info(&no_grant);
    let invited_info = info(&invited);
    let expired_info = info(&expired);
    let revoked_info = info(&revoked);
    let legacy_info = info(&legacy);

    // (1) Own session: joinable by app access, launch_context and name intact.
    assert_eq!(own_info["is_mine"], true);
    assert_eq!(own_info["joinable"], true);
    assert_eq!(own_info["launch_context"], launch);
    assert_eq!(own_info["name"], "My notes");

    // (2) No grant: hidden in the UI, context nulled — even with app access.
    assert_eq!(no_grant_info["is_mine"], false);
    assert_eq!(no_grant_info["joinable"], false);
    assert_eq!(no_grant_info["launch_context"], json!(null));
    assert_eq!(no_grant_info["name"], json!(null));

    // (3) Named invite: joinable without app access, context revealed (the
    // caller can open the app anyway, so it is not sensitive).
    assert_eq!(invited_info["is_mine"], false);
    assert_eq!(invited_info["joinable"], true);
    assert_eq!(invited_info["launch_context"], launch);
    assert_eq!(invited_info["name"], "Ghost invited");

    // (4) Expired invite: grant gone, context nulled.
    assert_eq!(expired_info["is_mine"], false);
    assert_eq!(expired_info["joinable"], false);
    assert_eq!(expired_info["launch_context"], json!(null));
    assert_eq!(expired_info["name"], json!(null));

    // (5) Revoked invite: grant gone, context nulled.
    assert_eq!(revoked_info["is_mine"], false);
    assert_eq!(revoked_info["joinable"], false);
    assert_eq!(revoked_info["launch_context"], json!(null));
    assert_eq!(revoked_info["name"], json!(null));

    // (6) Grandfathered participant entry (no live link): still joinable,
    // context revealed — until the owner revokes the entry.
    assert_eq!(legacy_info["is_mine"], false);
    assert_eq!(legacy_info["joinable"], true);
    assert_eq!(legacy_info["launch_context"], launch);
    assert_eq!(legacy_info["name"], "Ghost legacy");
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
    // ANT-472: the owner expressly invites the joiner via a named share link.
    let ghost = second_user(&sc, "ghost", "ghostpass123").await;
    create_share(&ghost, &other, &json!({ "role": "full", "username": "admin" })).await;

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
    // ANT-472: the owner expressly invites the joiner via a named share link.
    let ghost = second_user(&sc, "ghost", "ghostpass123").await;
    create_share(&ghost, &other, &json!({ "role": "full", "username": "admin" })).await;

    // Join, then follow the hop exactly the way the browser does. The client
    // sends an E2EE-encrypted empty body via secureFetch; the F4 join gate
    // (ANT-469) requires it.
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
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

/// ANT-472: the owner's express grant — not the caller's app access — is the
/// sole join gate. This case has app access but no grant: 403, and the
/// rejected request minted no participant token.
#[tokio::test]
async fn test_join_with_app_access_but_no_grant_forbidden() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // An app admin can use; the owner granted admin nothing.
    let app_id = seed_app(&sc, "shared-app", &["all"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{data:?}");
    assert_eq!(
        data.unwrap()["detail"],
        "The session owner has not granted you access."
    );
    assert_no_joiner_viewers(&sc, &other).await;
}

/// ANT-472: a live named invite is enough even when the invitee has no
/// entry in the app's allowlist (the grant is the sole gate).
#[tokio::test]
async fn test_join_invited_without_app_access() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // An app only ghost can use — admin has no app access.
    let app_id = seed_app(&sc, "ghost-only-app", &["ghost"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;
    let ghost = second_user(&sc, "ghost", "ghostpass123").await;
    create_share(&ghost, &other, &json!({ "role": "full", "username": "admin" })).await;

    // The invite lights the row up on the apps page...
    let (status, data) = sc.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let info = data
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == other)
        .unwrap();
    assert_eq!(info["joinable"], true);

    // ...and the join succeeds (the grant is the sole gate).
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "join should succeed: {data:?}");
    let url = data.as_ref().unwrap()["session_url"].as_str().unwrap().to_string();
    assert_eq!(url, format!("/join/{other}/admin"), "{url}");
    // And the joiner is recorded as a participant.
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
    assert!(
        viewers.iter().any(|v| {
            v.get("username").and_then(|u| u.as_str()) == Some("admin")
                && v.get("permission").and_then(|p| p.as_str()) == Some("participant")
        }),
        "joiner must have a participant entry: {viewers:?}"
    );
}

/// ANT-472: revoking the named invite removes the grant (the invitee has no
/// participant entry yet, so nothing else grants access).
#[tokio::test]
async fn test_join_revoked_invite_forbidden() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let app_id = seed_app(&sc, "shared-app", &["all"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;
    let ghost = second_user(&sc, "ghost", "ghostpass123").await;
    let share_id = create_share(&ghost, &other, &json!({ "role": "full", "username": "admin" }))
        .await;
    let (status, data) = ghost
        .call("DELETE", &format!("/api/sessions/{other}/shares/{share_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "revoke: {data:?}");
    // The revocation response names the invitee who just lost access.
    let revoked_info = data.expect("revoked share body");
    assert_eq!(revoked_info["invited_username"], "admin");
    assert_eq!(revoked_info["revoked"], true);

    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{data:?}");
    assert_eq!(
        data.unwrap()["detail"],
        "The session owner has not granted you access."
    );
}

/// ANT-472: an expired named invite no longer grants access.
#[tokio::test]
async fn test_join_expired_invite_forbidden() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let app_id = seed_app(&sc, "shared-app", &["all"]).await;
    let other = seed_session(&sc, "ghost", json!({ "provider_app_id": app_id })).await;
    let ghost = second_user(&sc, "ghost", "ghostpass123").await;
    let share_id = create_share(
        &ghost,
        &other,
        &json!({ "role": "full", "username": "admin", "expiry_hours": 1 }),
    )
    .await;
    {
        let mut shares = sc.state.session_shares.write().await;
        if let Some(meta) = shares.get_mut(&share_id) {
            meta.expiry_timestamp = Some(1.0);
        }
    }
    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{data:?}");
    assert_eq!(
        data.unwrap()["detail"],
        "The session owner has not granted you access."
    );
}

/// ANT-472: a persisted participant entry (a grandfathered v0.8.4
/// self-serve joiner, with no live invite link) is still a grant: the row is
/// visible and the re-join is idempotent.
#[tokio::test]
async fn test_join_grandfathered_participant_entry() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let app_id = seed_app(&sc, "shared-app", &["all"]).await;
    let other = seed_session(
        &sc,
        "ghost",
        json!({
            "provider_app_id": app_id,
            "viewers": [{
                "token": "legacy-tok-1",
                "slot": null,
                "mk_control": false,
                "username": "admin",
                "permission": "participant",
            }],
        }),
    )
    .await;

    let (status, data) = sc.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let info = data
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == other)
        .unwrap();
    assert_eq!(info["joinable"], true, "the entry is a standing grant");

    let (status, data) = sc
        .call("POST", &format!("/api/sessions/{other}/join"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "join should succeed: {data:?}");
    let url = data.as_ref().unwrap()["session_url"].as_str().unwrap().to_string();
    assert_eq!(url, format!("/join/{other}/admin"), "{url}");
    // The existing entry is reused: the re-join reuses the legacy token and
    // mints no second one.
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
    assert_eq!(viewers.len(), 1, "re-join must not mint a second token");
    assert_eq!(
        viewers[0].get("token").and_then(|t| t.as_str()),
        Some("legacy-tok-1")
    );
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
// F4-followup (ANT-444 / ANT-471): recreate + stop enforce the E2EE body
// (CSRF gate), mirroring the join POST gate above. Both routes authenticated
// with the SameSite=Lax login cookie but ignored their E2EE body, so a
// cross-site top-level form POST from a logged-in victim could stop or
// recreate their own sessions (the response body is E2EE-encrypted, so the
// attacker learns only the status). Requiring the AES-256-GCM request body is
// the gate: the normal client flow (secureFetch with an encrypted empty body)
// passes, a cross-site request without a crypto session is rejected 400.
// ---------------------------------------------------------------------------

/// Assert a rejected stop request did not remove the session.
async fn assert_session_still_present(sc: &SecureClient, sid: &str) {
    assert!(
        sc.state.sessions.read().await.contains_key(sid),
        "session must not be stopped by a rejected request"
    );
}

/// Assert a rejected recreate request did not populate the container registry
/// (i.e. `recreate_session` never ran — it would seed the fallback registry).
async fn assert_session_not_recreated(sc: &SecureClient, sid: &str) {
    let sessions = sc.state.sessions.read().await;
    let registry = sessions
        .get(sid)
        .and_then(|s| s.get("container_registry"));
    let empty = registry
        .and_then(|r| r.as_object())
        .map(|o| o.is_empty())
        .unwrap_or(true);
    assert!(
        empty,
        "container_registry must stay empty (no recreate): {registry:?}"
    );
}

#[tokio::test]
async fn test_recreate_form_post_without_e2ee_body_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    // The victim is logged in (the SameSite=Lax auth cookie rides along on a
    // top-level form POST) but has no crypto session: a form cannot carry
    // the X-Session-ID header.
    sc.http.login("admin", "admin1234").await;
    let sid = seed_session(&sc, "admin", json!({})).await;

    let resp = sc
        .http
        .post_form(&format!("/api/sessions/{sid}/recreate"), "x")
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "cross-site form POST must be rejected 4xx: {:?}",
        resp.body
    );
    assert_session_not_recreated(&sc, &sid).await;
}

#[tokio::test]
async fn test_recreate_xhr_without_e2ee_body_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    let sid = seed_session(&sc, "admin", json!({})).await;

    // Cross-site XHR: the auth cookie rides along (e.g. where Lax is relaxed
    // by ITP-style heuristics), the body is plain JSON, and no X-Session-ID.
    let resp = sc
        .http
        .post_json(&format!("/api/sessions/{sid}/recreate"), &json!({}))
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "cross-site XHR must be rejected 4xx: {:?}",
        resp.body
    );
    assert_session_not_recreated(&sc, &sid).await;
}

#[tokio::test]
async fn test_recreate_plaintext_body_with_crypto_session_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(&sc, "admin", json!({})).await;

    // A crypto session exists (X-Session-ID set), but the body is not the
    // AES-256-GCM envelope: decryption must fail and the request be rejected.
    let mut headers = std::collections::HashMap::new();
    headers.insert("x-session-id".to_string(), sc.session_id.clone().unwrap());
    let resp = sc
        .http
        .request(
            "POST",
            &format!("/api/sessions/{sid}/recreate"),
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
    assert_session_not_recreated(&sc, &sid).await;
}

#[tokio::test]
async fn test_stop_form_post_without_e2ee_body_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    let sid = seed_session(&sc, "admin", json!({})).await;

    // The stop route is DELETE; a cross-site form / XHR carries no
    // X-Session-ID header, so the gate rejects it before anything changes.
    let resp = sc
        .http
        .request(
            "DELETE",
            &format!("/api/sessions/{sid}"),
            Some("x".as_bytes().to_vec()),
            Some("application/x-www-form-urlencoded"),
            &std::collections::HashMap::new(),
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "cross-site form POST must be rejected 4xx: {:?}",
        resp.body
    );
    assert_session_still_present(&sc, &sid).await;
}

#[tokio::test]
async fn test_stop_xhr_without_e2ee_body_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    let sid = seed_session(&sc, "admin", json!({})).await;

    let resp = sc
        .http
        .request(
            "DELETE",
            &format!("/api/sessions/{sid}"),
            Some("{}".as_bytes().to_vec()),
            Some("application/json"),
            &std::collections::HashMap::new(),
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "cross-site XHR must be rejected 4xx: {:?}",
        resp.body
    );
    assert_session_still_present(&sc, &sid).await;
}

#[tokio::test]
async fn test_stop_plaintext_body_with_crypto_session_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(&sc, "admin", json!({})).await;

    let mut headers = std::collections::HashMap::new();
    headers.insert("x-session-id".to_string(), sc.session_id.clone().unwrap());
    let resp = sc
        .http
        .request(
            "DELETE",
            &format!("/api/sessions/{sid}"),
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
    assert_session_still_present(&sc, &sid).await;
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
        Some(json!({
            "ready": false,
            "detail": "No upstream address recorded.",
            "launch_failed": false,
            "launch_error": null
        }))
    );
}

#[tokio::test]
async fn test_session_status_recorded_ready_wins_over_probe() {
    // ANT-654: a session the background launch task confirmed ready answers
    // ready immediately, even though a live probe of this row (no ip/port)
    // would say otherwise.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(&sc, "admin", json!({ "ready": true })).await;
    let (status, data) = sc
        .call("GET", &format!("/api/sessions/{sid}/status"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["ready"], true);
    assert_eq!(data.as_ref().unwrap()["detail"], "ready");
    assert_eq!(data.as_ref().unwrap()["launch_failed"], false);
}

#[tokio::test]
async fn test_session_status_recorded_launch_failed_surfaces_error() {
    // ANT-654: a background start failure is surfaced through the status
    // endpoint — the recorded error detail, not a live-probe guess.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(
        &sc,
        "admin",
        json!({ "ready": false, "launch_failed": true, "launch_error": "boom" }),
    )
    .await;
    let (status, data) = sc
        .call("GET", &format!("/api/sessions/{sid}/status"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["ready"], false);
    assert_eq!(data.as_ref().unwrap()["detail"], "boom");
    assert_eq!(data.as_ref().unwrap()["launch_failed"], true);
    assert_eq!(data.as_ref().unwrap()["launch_error"], "boom");
}

#[tokio::test]
async fn test_session_status_recorded_not_ready_still_probes() {
    // ANT-654: a session still waiting on the background readiness task
    // reports not-ready with the live-probe detail and no failure flag.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(
        &sc,
        "admin",
        json!({ "ready": false, "ip": "127.0.0.1", "port": 9 }),
    )
    .await;
    let (status, data) = sc
        .call("GET", &format!("/api/sessions/{sid}/status"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["ready"], false);
    assert!(data.as_ref().unwrap()["detail"]
        .as_str()
        .unwrap()
        .starts_with("unreachable ("));
    assert_eq!(data.as_ref().unwrap()["launch_failed"], false);
    assert_eq!(data.as_ref().unwrap()["launch_error"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_session_root_not_ready_serves_csp_safe_waiting_screen() {
    // ANT-671: the pre-ready waiting screen ships inline script and style
    // (the 4 s auto-reload timer). It must carry a CSP that admits
    // 'unsafe-inline', or the strict default headers silently block the
    // reload and the page never flips to the app.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let sid = seed_session(
        &sc,
        "admin",
        json!({ "ready": false, "ip": "127.0.0.1", "port": 9 }),
    )
    .await;
    let tok = sc
        .state
        .sessions
        .read()
        .await
        .get(&sid)
        .unwrap()
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let mut headers = std::collections::HashMap::new();
    headers.insert("host".to_string(), "127.0.0.1".to_string());
    let hop = sc
        .http
        .get_with(&format!("/api/apps/session/{sid}/?access_token={tok}"), &headers)
        .await;
    assert_eq!(hop.status, StatusCode::SEE_OTHER, "body: {:?}", hop.body);
    // The browser follows the redirect carrying the session cookie the hop
    // just set; mirror that (TestClient has no cookie jar).
    let set_cookie = hop.header("set-cookie").unwrap_or_default();
    let cookie = set_cookie.split(';').next().unwrap_or_default().to_string();
    headers.insert("cookie".to_string(), cookie);
    let loc = hop.header("location").unwrap();
    let wait = sc.http.get_with(&loc, &headers).await;
    assert_eq!(wait.status, StatusCode::ACCEPTED, "body: {:?}", wait.body);
    let csp = wait.header("content-security-policy").unwrap_or_default();
    assert!(
        csp.contains("'unsafe-inline'"),
        "the waiting-screen CSP must admit inline script/style: {csp}"
    );
    let html = String::from_utf8_lossy(&wait.body);
    assert!(html.contains("location.reload()"), "body: {html}");
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
    // F4-followup (ANT-471): the client sends an E2EE-encrypted empty body
    // (`body: {}` in secureFetch); the gate requires it.
    let (status, data) = sc
        .call("DELETE", "/api/sessions/nope", Some(&json!({})))
        .await;
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
    let (status, data) = sc
        .call("DELETE", &format!("/api/sessions/{other}"), Some(&json!({})))
        .await;
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
    // F4-followup (ANT-471): the client sends an E2EE-encrypted empty body
    // (`body: {}` in secureFetch); the gate requires it.
    let (status, _) = sc
        .call("DELETE", &format!("/api/sessions/{sid}"), Some(&json!({})))
        .await;
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
