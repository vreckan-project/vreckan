//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Participant list, per-joiner revoke, and join-token TTL (ANT-467, F2):
//! the owner can see who joined a running session, revoke an individual
//! joiner without killing the session (the joiner drops on the next request,
//! including the container token table), and join grants age out on a TTL
//! via the background sweep.
//!
//! Drives the real axum router via the `common` harness; the app container
//! is faked with a recording HTTP upstream that accepts (and captures) the
//! selkies `/api/tokens` control-plane POSTs.

mod common;

use std::sync::{Arc, Mutex};

use common::{build_state, SecureClient, TempDir, TestClient};
use http::StatusCode;
use serde_json::{json, Value};
use vreckan::session_shares;
use vreckan::state::AppState;
use vreckan::user_manager;

// ---------------------------------------------------------------------------
// Fakes + shared helpers
// ---------------------------------------------------------------------------

/// A throwaway HTTP upstream that 200s every request and records the body
/// of every request whose path targets the selkies token control plane
/// (`.../api/tokens` or `/tokens`), so tests can assert on the pushed token
/// tables.
async fn spawn_recording_upstream() -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tables: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let tables = tables.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let tables = tables.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 65536];
                    let mut n = 0usize;
                    // Read the request head, then the body up to its
                    // Content-Length.
                    let mut want = 0usize;
                    loop {
                        if n >= want && want > 0 {
                            break;
                        }
                        match socket.read(&mut buf[n..]).await {
                            Ok(0) => break,
                            Ok(k) => {
                                n += k;
                                if want == 0 {
                                    if let Some(head_end) =
                                        std::str::from_utf8(&buf[..n])
                                            .unwrap_or("")
                                            .find("\r\n\r\n")
                                    {
                                        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
                                        let len = head
                                            .lines()
                                            .find_map(|l| {
                                                l.to_ascii_lowercase()
                                                    .strip_prefix("content-length:")
                                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                            })
                                            .unwrap_or(0);
                                        want = head_end + 4 + len;
                                        if want <= n {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    let head_end = raw.find("\r\n\r\n").unwrap_or(0);
                    let head = &raw[..head_end];
                    let body = &raw[head_end + 4..];
                    let is_tokens = head.contains("/api/tokens")
                        || head.lines().next().is_some_and(|l| {
                            l.contains("POST") && l.contains("/tokens")
                        });
                    if is_tokens {
                        tables.lock().unwrap().push(body.to_string());
                    }
                    let resp_body = b"<html>upstream</html>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                        resp_body.len(),
                        std::str::from_utf8(resp_body).unwrap()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        })
    };
    (format!("127.0.0.1:{}", addr.port()), tables)
}

/// A throwaway upstream that 404s the selkies token endpoint (older images
/// without the control plane) but 200s everything else — the legacy-mode
/// fallback path.
async fn spawn_legacy_upstream() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let mut n = 0usize;
                while n < buf.len() {
                    match socket.read(&mut buf[n..]).await {
                        Ok(0) => break,
                        Ok(k) => {
                            n += k;
                            if std::str::from_utf8(&buf[..n]).unwrap_or("").contains("\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
                let is_tokens = head.contains("/tokens");
                let (status, body): (&str, &[u8]) = if is_tokens {
                    ("404 Not Found", b"not in secure mode")
                } else {
                    ("200 OK", b"<html>upstream</html>")
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    std::str::from_utf8(body).unwrap()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("127.0.0.1:{}", addr.port())
}

/// Seed a plain session owned by the bootstrap admin, pointed at
/// `upstream` (host:port), for the installed app `app_id`.
async fn seed_plain_session(
    state: &AppState,
    upstream: &str,
    app_id: &str,
) -> (String, String) {
    let (ip, port) = upstream.rsplit_once(':').unwrap();
    let access_token = format!("access-{}", uuid::Uuid::new_v4());
    let sid = uuid::Uuid::new_v4().to_string();
    let session = json!({
        "instance_id": "ctr-1",
        "ip": ip,
        "port": port.parse::<i64>().unwrap(),
        "created_at": 1.0,
        "access_token": access_token,
        "is_collaboration": false,
        "username": "admin",
        "app_name": "Test App",
        "provider_app_id": app_id,
        "viewers": [],
        "container_registry": {},
    });
    state.sessions.write().await.insert(sid.clone(), session);
    (sid, access_token)
}

/// Insert an installed app that every user may access (the join route
/// checks the caller's access to the session's app).
async fn seed_app(sc: &SecureClient) -> String {
    let data = json!({
        "id": "join-app",
        "name": "Join App",
        "logo": "x",
        "url": "x",
        "source": "Test Store",
        "source_app_id": "join-app",
        "provider": "docker",
        "home_directories": true,
        "users": ["all"],
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

/// A SecureClient for a freshly created user (the joiner), sharing the
/// owner's app instance.
async fn joiner_client(state: &AppState) -> SecureClient {
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    user_manager::set_password(&state.db, "alice", "alicepass1").await.unwrap();
    let mut sc = SecureClient::new(state.clone());
    sc.login("alice", "alicepass1").await;
    sc.handshake().await;
    sc
}

/// The token out of a join response's session URL.
fn join_token(data: &Value) -> String {
    let url = data["session_url"].as_str().expect("session_url");
    url.rsplit_once("token=").expect("token param").1.to_string()
}

/// The session's last pushed token table (as captured by the recording
/// upstream), decoded.
fn last_pushed_table(tables: &Arc<Mutex<Vec<String>>>) -> Value {
    let captured = tables.lock().unwrap();
    let last = captured.last().expect("a token table was pushed");
    serde_json::from_str(last).expect("token table JSON")
}

// ---------------------------------------------------------------------------
// Participant list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_participants_list_shows_joiner() {
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let (upstream, _tables) = spawn_recording_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;
    let joiner = joiner_client(&owner.state).await;

    // alice self-serves a join into the owner's running session.
    let (status, data) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "join should succeed: {data:?}");

    // The session list flags the joiner on the owner's session.
    let (status, data) = owner.call("GET", "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let entry = data
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid)
        .unwrap();
    assert_eq!(entry["is_mine"], true);
    assert_eq!(entry["participants"], json!(["alice"]));

    // The participants endpoint lists the joiner with the join source.
    let (status, data) = owner
        .call("GET", &format!("/api/sessions/{sid}/participants"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let participants = data.unwrap();
    let list = participants.as_array().unwrap();
    assert_eq!(list.len(), 1, "exactly the joiner: {participants:?}");
    assert_eq!(list[0]["username"], "alice");
    assert_eq!(list[0]["source"], "join");
    assert_eq!(list[0]["share_id"], "join:alice");
    assert!(
        list[0]["expiry"].is_number(),
        "join grants carry a TTL: {list:?}"
    );
    assert!(list[0]["joined_at"].is_number());
}

#[tokio::test]
async fn test_participants_auth() {
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let (upstream, _tables) = spawn_recording_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;

    // Unauthenticated: plaintext 401 (no E2EE session yet). A fresh
    // client: the owner's raw client would already carry the login
    // cookie and hit the E2EE success path instead.
    let anon = TestClient::new(owner.state.clone());
    let resp = anon.get(&format!("/api/sessions/{sid}/participants")).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);

    // The joiner is not the owner.
    let joiner = joiner_client(&owner.state).await;
    let (status, data) = joiner
        .call("GET", &format!("/api/sessions/{sid}/participants"), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{data:?}");

    // The owner on an unknown session.
    let (status, data) = owner
        .call("GET", "/api/sessions/does-not-exist/participants", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{data:?}");
}

// ---------------------------------------------------------------------------
// Per-joiner revoke
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_revoke_joiner_drops_on_next_request() {
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let (upstream, tables) = spawn_recording_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;
    let joiner = joiner_client(&owner.state).await;

    let (status, data) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let token = join_token(data.as_ref().unwrap());

    // The join promoted the session and pushed the joiner into the
    // container's token table.
    let session = owner.state.sessions.read().await.get(&sid).cloned().unwrap();
    assert!(
        session
            .get("master_token")
            .and_then(|v| v.as_str())
            .is_some_and(|m| !m.is_empty()),
        "session must be promoted"
    );
    let pushed = last_pushed_table(&tables);
    assert!(
        pushed.get(&token).is_some(),
        "joiner's token in the pushed table: {pushed:?}"
    );
    // The joiner's token is admitted by the proxy before the revoke.
    let ok = owner
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(ok.status, StatusCode::OK, "body: {:?}", ok.body);

    // The owner revokes just alice — the session and its master token
    // survive.
    let (status, data) = owner
        .call("DELETE", &format!("/api/sessions/{sid}/participants/alice"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{data:?}");

    // Stripped from the vreckan-side token map and the persisted viewers
    // list.
    assert!(
        owner
            .state
            .session_share_tokens
            .read()
            .await
            .get(&token)
            .is_none(),
        "token removed from the vreckan-side map"
    );
    let session = owner.state.sessions.read().await.get(&sid).cloned().unwrap();
    let viewers = session
        .get("viewers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !viewers
            .iter()
            .any(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str())),
        "joiner removed from the session viewers list: {viewers:?}"
    );
    assert!(
        session
            .get("master_token")
            .and_then(|v| v.as_str())
            .is_some_and(|m| !m.is_empty()),
        "the session itself is not killed by the revoke"
    );
    // The container token table was re-pushed without the joiner.
    let pushed = last_pushed_table(&tables);
    assert!(
        pushed.get(&token).is_none(),
        "joiner's token stripped from the container table: {pushed:?}"
    );

    // The joiner drops on the next request.
    let denied = owner
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "body: {:?}", denied.body);

    // A re-join mints a fresh token (the old one stays dead).
    let (status, data2) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data2:?}");
    let token2 = join_token(data2.as_ref().unwrap());
    assert_ne!(token2, token, "re-join after revoke mints a new token");
    let admitted = owner
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token2}"))
        .await;
    assert_eq!(admitted.status, StatusCode::OK, "body: {:?}", admitted.body);
    let denied_again = owner
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(
        denied_again.status,
        StatusCode::FORBIDDEN,
        "the revoked token does not come back"
    );
}

#[tokio::test]
async fn test_revoke_unknown_and_double_joiner() {
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let (upstream, _tables) = spawn_recording_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;
    let joiner = joiner_client(&owner.state).await;
    let (status, data) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");

    // A user that never joined.
    let (status, data) = owner
        .call("DELETE", &format!("/api/sessions/{sid}/participants/ghost"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{data:?}");

    // Revoking works once; the second revoke is 404.
    let (status, data) = owner
        .call("DELETE", &format!("/api/sessions/{sid}/participants/alice"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{data:?}");
    let (status, data) = owner
        .call("DELETE", &format!("/api/sessions/{sid}/participants/alice"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{data:?}");
}

#[tokio::test]
async fn test_revoke_joiner_on_legacy_session() {
    // A legacy (non-secure) session knows the joiner only through the
    // vreckan-side token map — the revoke must strip it there, which is the
    // only admission path for this session kind.
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let upstream = spawn_legacy_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;
    let joiner = joiner_client(&owner.state).await;

    let (status, data) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let token = join_token(data.as_ref().unwrap());
    let session = owner.state.sessions.read().await.get(&sid).cloned().unwrap();
    assert!(
        session
            .get("master_token")
            .map(|m| m.is_null() || m.as_str().map(|s| s.is_empty()).unwrap_or(true))
            .unwrap_or(true),
        "legacy session stays non-secure: {session:?}"
    );
    let ok = owner
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(ok.status, StatusCode::OK, "body: {:?}", ok.body);

    let (status, data) = owner
        .call("DELETE", &format!("/api/sessions/{sid}/participants/alice"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{data:?}");
    let denied = owner
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "body: {:?}", denied.body);
}

// ---------------------------------------------------------------------------
// Join-token TTL
// ---------------------------------------------------------------------------

/// Backdate the joiner's grant so the sweep sees it as expired: the viewer
/// entry's TTL and the in-memory token record's TTL.
async fn expire_join(state: &AppState, sid: &str, token: &str) {
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as f64)
        - 60.0;
    {
        let mut sessions = state.sessions.write().await;
        let session = sessions.get_mut(sid).unwrap();
        if let Some(viewers) = session.get_mut("viewers").and_then(|v| v.as_array_mut()) {
            if let Some(entry) = viewers
                .iter_mut()
                .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token))
            {
                entry["expiry"] = Value::from(now);
            }
        }
    }
    // Outside the write guard: `save_sessions` re-locks the map read-only
    // (tokio locks are not reentrant).
    vreckan::catalog::save_sessions(state).await;
    let mut map = state.session_share_tokens.write().await;
    if let Some(record) = map.get_mut(token) {
        record.expiry = Some(now);
    }
}

#[tokio::test]
async fn test_join_ttl_sweep_expires_grant() {
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let (upstream, _tables) = spawn_recording_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;
    let joiner = joiner_client(&owner.state).await;

    let (status, data) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let token = join_token(data.as_ref().unwrap());
    let ok = owner
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(ok.status, StatusCode::OK, "body: {:?}", ok.body);

    // The TTL runs out; the background sweep strips the grant the same way
    // an owner revocation would.
    expire_join(&owner.state, &sid, &token).await;
    session_shares::sweep_expired_session_joins(&owner.state).await;

    assert!(
        owner
            .state
            .session_share_tokens
            .read()
            .await
            .get(&token)
            .is_none(),
        "expired join token pruned from the vreckan-side map"
    );
    let session = owner.state.sessions.read().await.get(&sid).cloned().unwrap();
    let viewers = session
        .get("viewers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !viewers
            .iter()
            .any(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str())),
        "expired join entry removed from the persisted viewers list"
    );
    let denied = owner
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "body: {:?}", denied.body);
}

#[tokio::test]
async fn test_rejoin_after_expiry_refreshes_ttl() {
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let (upstream, _tables) = spawn_recording_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;
    let joiner = joiner_client(&owner.state).await;

    let (status, data) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let token = join_token(data.as_ref().unwrap());

    // An expired join is refreshed in place on re-join: same token, fresh
    // TTL — no second grant, no second viewers entry.
    expire_join(&owner.state, &sid, &token).await;
    let (status, data2) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data2:?}");
    let token2 = join_token(data2.as_ref().unwrap());
    assert_eq!(token2, token, "re-join reuses the existing token");
    let session = owner.state.sessions.read().await.get(&sid).cloned().unwrap();
    let viewers = session
        .get("viewers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(viewers.len(), 1, "no second entry: {viewers:?}");
    let entry = &viewers[0];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as f64;
    let expiry = entry["expiry"].as_f64().expect("refreshed expiry");
    assert!(expiry > now, "TTL refreshed into the future: {expiry}");
    let token_map = owner.state.session_share_tokens.read().await;
    let record = token_map.get(&token).expect("token record");
    assert!(
        record.expiry.is_some_and(|e| e > now),
        "token record carries the refreshed TTL: {record:?}"
    );
}

#[tokio::test]
async fn test_rejoin_backfills_ttl_for_pre_085_grants() {
    // A join persisted before 0.8.5 has no expiry at all; the first
    // re-join after the upgrade backfills the TTL instead of letting the
    // grant stay permanent forever.
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let (upstream, _tables) = spawn_recording_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;
    let joiner = joiner_client(&owner.state).await;

    let (status, data) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let token = join_token(data.as_ref().unwrap());

    // Simulate the pre-0.8.5 shape: drop the expiry key from the entry and
    // the TTL from the token record.
    {
        let mut sessions = owner.state.sessions.write().await;
        let session = sessions.get_mut(&sid).unwrap();
        if let Some(viewers) = session
            .get_mut("viewers")
            .and_then(|v| v.as_array_mut())
        {
            if let Some(entry) = viewers
                .iter_mut()
                .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str()))
            {
                entry.as_object_mut().unwrap().remove("expiry");
                entry.as_object_mut().unwrap().remove("share_id");
                entry.as_object_mut().unwrap().remove("joined_at");
            }
        }
    }
    // Outside the write guard: `save_sessions` re-locks the map read-only
    // (tokio locks are not reentrant).
    vreckan::catalog::save_sessions(&owner.state).await;
    // Scoped: the re-join handler below takes the map's write lock, and a
    // guard held here would deadlock it (tokio locks are not reentrant).
    {
        let mut map = owner.state.session_share_tokens.write().await;
        if let Some(record) = map.get_mut(&token) {
            record.expiry = None;
            record.created_at = 1.0;
        }
    }

    let (status, data2) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data2:?}");
    let session = owner.state.sessions.read().await.get(&sid).cloned().unwrap();
    let entry = session
        .get("viewers")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str()))
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as f64;
    assert!(
        entry["expiry"].as_f64().is_some_and(|e| e > now),
        "TTL backfilled on the first post-upgrade re-join: {entry:?}"
    );
}

#[tokio::test]
async fn test_join_ttl_constant() {
    // The TTL is a full day; the join grant's expiry sits 24h past the
    // grant (within the same minute the test observes it).
    let _root = TempDir::new();
    let mut owner = SecureClient::new(build_state(_root.path()).await);
    owner.http.login("admin", "admin1234").await;
    owner.handshake().await;
    let (upstream, _tables) = spawn_recording_upstream().await;
    let app_id = seed_app(&owner).await;
    let (sid, _) = seed_plain_session(&owner.state, &upstream, &app_id).await;
    let joiner = joiner_client(&owner.state).await;

    let (status, data) = joiner
        .call("POST", &format!("/api/sessions/{sid}/join"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let token = join_token(data.as_ref().unwrap());
    let session = owner.state.sessions.read().await.get(&sid).cloned().unwrap();
    let entry = session
        .get("viewers")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str()))
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as f64;
    let (joined_at, expiry) = (entry["joined_at"].as_f64().unwrap(), entry["expiry"].as_f64().unwrap());
    assert!(expiry - joined_at >= 23.9 * 3600.0, "TTL is ~24h: {expiry} - {joined_at}");
    assert!(expiry > now, "the fresh grant is not yet expired");
    assert_eq!(vreckan::session_shares::JOIN_TTL_HOURS, 24);
}
