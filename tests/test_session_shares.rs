//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Session-share links (ANT-305, WP2): the `session_shares` durable store,
//! the owner management API (`/api/sessions/{id}/shares`), the account-less
//! public entry (`GET/POST /share/{id}`), per-visit tokens, lazy
//! secure-mode promotion (with the legacy `?role=` fallback), and the
//! cleanup job.
//!
//! Drives the real axum router (via the `common` harness); app containers
//! are faked with throwaway HTTP upstreams that answer (or refuse) the
//! selkies `/api/tokens` control-plane POST.

mod common;

use std::collections::HashMap;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;
use vreckan::state::AppState;

// ---------------------------------------------------------------------------
// Fakes + shared helpers
// ---------------------------------------------------------------------------

/// A throwaway HTTP upstream that answers 200 to every request — stands in
/// for a selkies container whose token endpoint accepts the pushed table.
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

/// A throwaway upstream that 404s the selkies token endpoint (older images
/// without the control plane) but 200s everything else — the legacy-mode
/// fallback path.
async fn spawn_legacy_upstream() -> (String, tokio::task::JoinHandle<()>) {
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
                let mut n = 0usize;
                // Read until the request head is in so the path can be seen.
                while n < buf.len() {
                    match socket.read(&mut buf[n..]).await {
                        Ok(0) => break,
                        Ok(k) => n += k,
                        Err(_) => break,
                    }
                    if std::str::from_utf8(&buf[..n]).unwrap_or("").contains("\r\n\r\n") {
                        break;
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
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// Seed a plain (non-collaboration) session owned by the bootstrap admin,
/// pointed at `upstream` (host:port).
async fn seed_plain_session(state: &AppState, upstream: &str) -> (String, String) {
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
        "viewers": [],
        "container_registry": {},
    });
    state.sessions.write().await.insert(sid.clone(), session);
    (sid, access_token)
}

/// Seed a collaboration (room) session owned by the bootstrap admin, with the
/// room's master token, pointed at `upstream` (host:port).
async fn seed_room_session(state: &AppState, upstream: &str) -> (String, String, String) {
    let (ip, port) = upstream.rsplit_once(':').unwrap();
    let access_token = format!("access-{}", uuid::Uuid::new_v4());
    let controller_token = format!("ctrl-{}", uuid::Uuid::new_v4());
    let master_token = format!("master-{}", uuid::Uuid::new_v4());
    let sid = uuid::Uuid::new_v4().to_string();
    let session = json!({
        "instance_id": "ctr-1",
        "ip": ip,
        "port": port.parse::<i64>().unwrap(),
        "created_at": 1.0,
        "access_token": access_token,
        "controller_token": controller_token,
        "master_token": master_token,
        "is_collaboration": true,
        "username": "admin",
        "app_name": "Test App",
        "viewers": [],
        "container_registry": {},
    });
    state.sessions.write().await.insert(sid.clone(), session);
    (sid, access_token, master_token)
}

/// A SecureClient logged in as the bootstrap admin with a completed E2EE
/// handshake.
async fn admin_client(root: &std::path::Path) -> SecureClient {
    let mut sc = SecureClient::new(build_state(root).await);
    sc.login("admin", "admin1234").await;
    sc.handshake().await;
    sc
}

/// The `Location` header of a response, as a string.
fn location(resp: &common::RawResponse) -> String {
    resp.header("location").unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Owner management API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_and_list_shares() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only", "expiry_hours": 12 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let created = body.expect("created share body");
    let share_id = created.get("share_id").and_then(|v| v.as_str()).unwrap().to_string();
    assert_eq!(created.get("role").and_then(|v| v.as_str()), Some("view_only"));
    assert_eq!(
        created.get("url").and_then(|v| v.as_str()),
        Some(format!("/share/{share_id}").as_str())
    );
    assert_eq!(created.get("has_password").and_then(|v| v.as_bool()), Some(false));
    // The standard view-only preset: browse + download, no input/upload.
    assert_eq!(
        created.get("caps").and_then(|v| v.get("input")).and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        created
            .get("caps")
            .and_then(|v| v.get("file_browse"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        created
            .get("caps")
            .and_then(|v| v.get("file_download"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        created
            .get("caps")
            .and_then(|v| v.get("file_upload"))
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert!(created.get("expiry_timestamp").and_then(|v| v.as_f64()).is_some());

    // A full share: every cap forced on.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full", "password": "s3cret" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create full: {body:?}");
    let full = body.expect("full share body");
    assert_eq!(full.get("has_password").and_then(|v| v.as_bool()), Some(true));
    for cap in ["input", "file_browse", "file_download", "file_upload"] {
        assert_eq!(
            full.get("caps").and_then(|v| v.get(cap)).and_then(|v| v.as_bool()),
            Some(true),
            "full share cap {cap}"
        );
    }

    // The list shows both, newest first.
    let (status, body) = sc
        .call("GET", &format!("/api/sessions/{sid}/shares"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "list: {body:?}");
    let list_body = body.expect("list body");
    let list = list_body.as_array().expect("list array");
    assert_eq!(list.len(), 2);
    assert_eq!(
        list[0].get("share_id").and_then(|v| v.as_str()),
        full.get("share_id").and_then(|v| v.as_str())
    );
    assert_eq!(
        list[1].get("share_id").and_then(|v| v.as_str()),
        Some(share_id.as_str())
    );
}

#[tokio::test]
async fn test_role_invariants_on_create_and_update() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    // view_only refuses input even when asked for it.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only", "caps": { "input": true } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let view_only = body.expect("body");
    assert_eq!(
        view_only.get("caps").and_then(|v| v.get("input")).and_then(|v| v.as_bool()),
        Some(false),
        "view_only must drop input"
    );
    let share_id = view_only
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Flipping to full turns every cap on.
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({ "role": "full" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "patch: {body:?}");
    let updated = body.expect("patched body");
    assert_eq!(updated.get("role").and_then(|v| v.as_str()), Some("full"));
    for cap in ["input", "file_browse", "file_download", "file_upload"] {
        assert_eq!(
            updated.get("caps").and_then(|v| v.get(cap)).and_then(|v| v.as_bool()),
            Some(true),
            "full cap {cap} after role flip"
        );
    }

    // Flipping back to view_only removes input but keeps the file caps.
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "patch back: {body:?}");
    let back = body.expect("body");
    assert_eq!(back.get("role").and_then(|v| v.as_str()), Some("view_only"));
    assert_eq!(
        back.get("caps").and_then(|v| v.get("input")).and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        back.get("caps")
            .and_then(|v| v.get("file_download"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "file caps survive a role flip"
    );

    // A bad role is rejected.
    let (status, _body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({ "role": "moderator" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_password_and_expiry_updates() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only", "password": "first" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let created = body.expect("body");
    let share_id = created
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    assert_eq!(
        created.get("has_password").and_then(|v| v.as_bool()),
        Some(true)
    );

    // Change the password.
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({ "password": "second" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "set pw: {body:?}");
    assert_eq!(
        body.expect("body")
            .get("has_password")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    // The new password now verifies at the public entry.
    let wrong = sc.http.post_form(&format!("/share/{share_id}"), "first").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "old password must fail");
    let right = sc.http.post_form(&format!("/share/{share_id}"), "second").await;
    assert_eq!(right.status, StatusCode::SEE_OTHER, "new password must work");

    // Clear the password.
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({ "password": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "clear pw: {body:?}");
    assert_eq!(
        body.expect("body")
            .get("has_password")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    // Expiry: set then clear.
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({ "expiry_hours": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "set expiry: {body:?}");
    assert!(
        body.expect("body")
            .get("expiry_timestamp")
            .and_then(|v| v.as_f64())
            .is_some()
    );
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({ "expiry_hours": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "clear expiry: {body:?}");
    // A cleared expiry serializes as `null` (the key stays present).
    assert!(body
        .expect("body")
        .get("expiry_timestamp")
        .and_then(|v| v.as_f64())
        .is_none());
}

#[tokio::test]
async fn test_revoke_share_and_unknown_ids() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Revoke.
    let (status, _) = sc
        .call("DELETE", &format!("/api/sessions/{sid}/shares/{share_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "revoke");
    let (status, body) = sc
        .call("GET", &format!("/api/sessions/{sid}/shares"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "list: {body:?}");
    let entry = body
        .expect("list")
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .expect("revoked share still listed");
    assert_eq!(entry.get("revoked").and_then(|v| v.as_bool()), Some(true));

    // Operations on unknown ids.
    let (status, _) = sc
        .call("DELETE", &format!("/api/sessions/{sid}/shares/nope"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = sc
        .call("PATCH", &format!("/api/sessions/{sid}/shares/nope"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = sc.call("GET", "/api/sessions/unknown-sid/shares", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_owner_must_match_session() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (_sid, _access) = seed_plain_session(&sc.state, &upstream).await;
    // A session owned by someone else.
    let other_sid = uuid::Uuid::new_v4().to_string();
    let other = json!({
        "instance_id": "ctr-2",
        "ip": "127.0.0.1",
        "port": 1,
        "access_token": "a",
        "is_collaboration": false,
        "username": "ghost-user",
        "viewers": [],
    });
    sc.state.sessions.write().await.insert(other_sid.clone(), other);

    let (status, _) = sc
        .call(
            "POST",
            &format!("/api/sessions/{other_sid}/shares"),
            Some(&json!({ "role": "full" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not the session owner");
}

// ---------------------------------------------------------------------------
// Public entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_public_entry_status_codes() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    // Unknown share.
    let resp = sc.http.get("/share/does-not-exist").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);

    // Create + revoke -> 410.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let (status, _) = sc
        .call("DELETE", &format!("/api/sessions/{sid}/shares/{share_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let resp = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(resp.status, StatusCode::GONE, "revoked share is 410");

    // Expired -> 410.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only", "expiry_hours": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create2: {body:?}");
    let share_id2 = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    {
        let mut shares = sc.state.session_shares.write().await;
        if let Some(meta) = shares.get_mut(&share_id2) {
            meta.expiry_timestamp = Some(1.0);
        }
    }
    let resp = sc.http.get(&format!("/share/{share_id2}")).await;
    assert_eq!(resp.status, StatusCode::GONE, "expired share is 410");

    // Session ended (removed from the live map) -> 410.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create3: {body:?}");
    let share_id3 = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    sc.state.sessions.write().await.remove(&sid);
    let resp = sc.http.get(&format!("/share/{share_id3}")).await;
    assert_eq!(resp.status, StatusCode::GONE, "ended session is 410");
}

#[tokio::test]
async fn test_password_page_flow() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only", "password": "letmein" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // GET presents the password page, pointed at this share's entry.
    let page = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(page.status, StatusCode::OK, "password page");
    let html = String::from_utf8_lossy(&page.body);
    assert!(html.contains(&format!("action=\"/share/{share_id}\"")), "form action: {html}");

    // Wrong password -> 401 + the page again.
    let wrong = sc.http.post_form(&format!("/share/{share_id}"), "nope").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(String::from_utf8_lossy(&wrong.body).contains("Incorrect password"));

    // Correct password -> 303 to the session.
    let right = sc.http.post_form(&format!("/share/{share_id}"), "letmein").await;
    assert_eq!(right.status, StatusCode::SEE_OTHER, "body: {:?}", right.body);
    let loc = location(&right);
    assert!(
        loc.starts_with(&format!("/api/apps/session/{sid}/?token=")),
        "redirect: {loc}"
    );
    // A non-password share POSTed directly gets a 400.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create2: {body:?}");
    let share_id2 = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let resp = sc.http.post_form(&format!("/share/{share_id2}"), "x").await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Per-visit grants: promotion, legacy fallback, rooms
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_plain_share_visit_promotes_session() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Visit: 303 to the session with the per-visit token.
    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER, "body: {:?}", grant.body);
    let loc = location(&grant);
    let token = loc
        .strip_prefix(&format!("/api/apps/session/{sid}/?token="))
        .and_then(|rest| rest.split('&').next())
        .expect("token in redirect: {loc}")
        .to_string();

    // The session was promoted: a master token exists, the visitor is in the
    // viewers list as a controller (full share), and the vreckan-side token
    // map records the share.
    let session = sc.state.sessions.read().await.get(&sid).cloned().unwrap();
    assert!(
        session
            .get("master_token")
            .and_then(|v| v.as_str())
            .is_some_and(|m| !m.is_empty()),
        "session must be promoted"
    );
    let viewers = session.get("viewers").and_then(|v| v.as_array()).unwrap();
    let entry = viewers
        .iter()
        .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str()))
        .expect("visitor in the viewers list");
    assert_eq!(
        entry.get("role").and_then(|v| v.as_str()),
        Some("controller"),
        "full share on a promoted plain session is a controller"
    );
    assert_eq!(entry.get("permission").and_then(|v| v.as_str()), Some("participant"));
    assert_eq!(
        sc.state.session_share_tokens.read().await.get(&token).map(|r| r.share_id.as_str()),
        Some(share_id.as_str())
    );

    // The visitor's token is accepted by the proxy, which sets the collab
    // cookie for the proxied page's sub-requests.
    let page = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(page.status, StatusCode::OK, "body: {:?}", page.body);
    let cookie_prefix = format!("collab_token_{sid}={token}");
    assert!(
        page
            .headers
            .get_all("set-cookie")
            .iter()
            .any(|v| v.to_str().ok().is_some_and(|s| s.starts_with(&cookie_prefix))),
        "first hit must set the collab cookie"
    );
    // A subsequent sub-request authenticated by the cookie also works.
    let mut headers = HashMap::new();
    headers.insert("cookie".to_string(), format!("collab_token_{sid}={token}"));
    let sub = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/file/list"), None, None, &headers)
        .await;
    assert_eq!(sub.status, StatusCode::OK, "cookie-authenticated sub-request");
}

#[tokio::test]
async fn test_plain_share_visit_legacy_fallback() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_legacy_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    // view_only on a container without the token control plane.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER, "body: {:?}", grant.body);
    let loc = location(&grant);
    let token = loc
        .strip_prefix(&format!("/api/apps/session/{sid}/?token="))
        .and_then(|rest| rest.split('&').next())
        .expect("token in redirect: {loc}")
        .to_string();
    assert!(
        loc.contains("&role=viewer"),
        "legacy view_only redirect must carry role=viewer: {loc}"
    );

    // The promotion was rolled back: no master token on the session.
    let session = sc.state.sessions.read().await.get(&sid).cloned().unwrap();
    assert!(
        session
            .get("master_token")
            .and_then(|v| v.as_str())
            .is_none_or(|m| m.is_empty()),
        "legacy session must stay un-promoted"
    );

    // The visitor token is still accepted (vreckan-side map) and proxied.
    let page = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(page.status, StatusCode::OK, "body: {:?}", page.body);

    // A full share on the same legacy container: no role param (the container
    // defaults to controller).
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create full: {body:?}");
    let share_id2 = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let grant2 = sc.http.get(&format!("/share/{share_id2}")).await;
    assert_eq!(grant2.status, StatusCode::SEE_OTHER);
    let loc2 = location(&grant2);
    assert!(
        !loc2.contains("role="),
        "legacy full share must not send a role param: {loc2}"
    );
}

#[tokio::test]
async fn test_room_share_visit() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access, _master) = seed_room_session(&sc.state, &upstream).await;

    // view_only share on a room: the visitor joins the room as readonly.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER, "body: {:?}", grant.body);
    let loc = location(&grant);
    let token = loc
        .strip_prefix(&format!("/room/{sid}?token="))
        .expect("room redirect: {loc}")
        .to_string();
    let session = sc.state.sessions.read().await.get(&sid).cloned().unwrap();
    let viewers = session.get("viewers").and_then(|v| v.as_array()).unwrap();
    let entry = viewers
        .iter()
        .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str()))
        .expect("room visitor in the viewers list");
    assert_eq!(entry.get("permission").and_then(|v| v.as_str()), Some("readonly"));
    // The room page admits the visitor.
    let room = sc.http.get(&format!("/room/{sid}?token={token}")).await;
    assert_eq!(room.status, StatusCode::OK, "body: {:?}", room.body);
    let html = String::from_utf8_lossy(&room.body);
    assert!(html.contains("\"userPermission\":\"readonly\""), "room page permission");

    // A full share on a room: participant permission (the room's MK/slot
    // mechanism provides the input).
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create full: {body:?}");
    let share_id2 = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let grant2 = sc.http.get(&format!("/share/{share_id2}")).await;
    assert_eq!(grant2.status, StatusCode::SEE_OTHER);
    let token2 = location(&grant2)
        .strip_prefix(&format!("/room/{sid}?token="))
        .expect("room redirect")
        .to_string();
    let session = sc.state.sessions.read().await.get(&sid).cloned().unwrap();
    let entry2 = session
        .get("viewers")
        .and_then(|v| v.as_array())
        .and_then(|v| v.iter().find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token2.as_str())))
        .expect("room visitor in the viewers list");
    assert_eq!(entry2.get("permission").and_then(|v| v.as_str()), Some("participant"));
}

#[tokio::test]
async fn test_revoke_strips_visitor_tokens() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER);
    let token = location(&grant)
        .strip_prefix(&format!("/api/apps/session/{sid}/?token="))
        .and_then(|rest| rest.split('&').next())
        .expect("token")
        .to_string();
    // The token works before revocation.
    let ok = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(ok.status, StatusCode::OK);

    // Revoke: the visitor token stops working (from the viewers list and the
    // vreckan-side map).
    let (status, _) = sc
        .call("DELETE", &format!("/api/sessions/{sid}/shares/{share_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        sc.state
            .session_share_tokens
            .read()
            .await
            .get(&token)
            .is_none(),
        "token removed from the vreckan-side map"
    );
    let session = sc.state.sessions.read().await.get(&sid).cloned().unwrap();
    let viewers = session.get("viewers").and_then(|v| v.as_array()).unwrap();
    assert!(
        !viewers
            .iter()
            .any(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str())),
        "visitor removed from the session viewers list"
    );
    let denied = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "revoked visitor is 403");
}

// ---------------------------------------------------------------------------
// Cleanup + persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cleanup_expired_session_shares() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    // One expired share (with a visitor token) and one live one.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full", "expiry_hours": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let expired_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let grant = sc.http.get(&format!("/share/{expired_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER);
    let token = location(&grant)
        .strip_prefix(&format!("/api/apps/session/{sid}/?token="))
        .and_then(|rest| rest.split('&').next())
        .expect("token")
        .to_string();
    {
        let mut shares = sc.state.session_shares.write().await;
        if let Some(meta) = shares.get_mut(&expired_id) {
            meta.expiry_timestamp = Some(1.0);
        }
    }
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create live: {body:?}");
    let live_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    vreckan::session_shares::cleanup_expired_session_shares(&sc.state).await;

    assert!(sc.state.session_shares.read().await.get(&expired_id).is_none());
    assert!(sc.state.session_shares.read().await.get(&live_id).is_some());
    assert!(
        sc.state
            .session_share_tokens
            .read()
            .await
            .get(&token)
            .is_none(),
        "the expired share's visitor token is stripped"
    );
}

#[tokio::test]
async fn test_session_share_load_save_roundtrip() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only", "password": "pw", "expiry_hours": 3 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Wipe the in-memory map, reload from the database: the share survives.
    sc.state.session_shares.write().await.clear();
    vreckan::session_shares::load_session_shares(&sc.state).await;
    let reloaded = sc
        .state
        .session_shares
        .read()
        .await
        .get(&share_id)
        .cloned()
        .expect("share reloaded from the database");
    assert_eq!(reloaded.session_id, sid);
    assert_eq!(reloaded.owner_username, "admin");
    assert_eq!(reloaded.role, "view_only");
    assert!(reloaded.password_hash.is_some());
    assert!(reloaded.expiry_timestamp.is_some());
    assert!(!reloaded.revoked);
}
