//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Session-share links (ANT-305, WP2): the `session_shares` durable store,
//! the owner management API (`/api/sessions/{id}/shares`), the account-less
//! public entry (`GET/POST /share/{id}`), per-visit tokens, lazy
//! secure-mode promotion (with the legacy `?role=` fallback), and the
//! cleanup job. WP3 (ANT-306): the session proxy's server-side enforcement
//! of a share's file capabilities (browse / download / upload), its
//! next-open cap semantics, and the owner/room-traffic exemptions.
//!
//! Drives the real axum router (via the `common` harness); app containers
//! are faked with throwaway HTTP upstreams that answer (or refuse) the
//! selkies `/api/tokens` control-plane POST and emulate the selkies web
//! client's file API (`api/files/...` listings/downloads, `api/upload`).

mod common;

use std::collections::HashMap;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;
use vreckan::state::AppState;
use vreckan::user_manager;

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

/// A raw HTTP response for the file-API fake.
fn files_resp(status: &str, extra_header: &str, body: &str) -> String {
    let extra_line = if extra_header.is_empty() {
        String::new()
    } else {
        format!("{extra_header}\r\n")
    };
    format!(
        "HTTP/1.1 {status}\r\n{extra_line}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A throwaway upstream that emulates the selkies web client's file API
/// (ANT-306, the surface enumerated in container `ant-306-endpoints`):
///
/// - `api/files` (the no-slash root) → 301 to the slash-terminated listing,
///   as the real container does
/// - `api/files/<dir>/` → 200 HTML listing (no `Content-Disposition`)
/// - `api/files/<dir>` (no trailing slash) → 301 to the slash-terminated
///   listing, as the real container does
/// - `api/files/<file>` → 200 bytes with `Content-Disposition: attachment`
/// - `api/upload` → 200
/// - `api/tokens` → 200 (a promotion-capable container)
///
/// Paths arrive with the session prefix the proxy forwards intact.
async fn spawn_files_upstream() -> (String, tokio::task::JoinHandle<()>) {
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
                // The request line's path, without its query string.
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .map(|p| p.split('?').next().unwrap_or(p))
                    .unwrap_or("");
                let resp = if path.ends_with("/api/files") {
                    // The no-slash root: the container 301s to the
                    // slash-terminated listing.
                    files_resp(
                        "301 Moved Permanently",
                        &format!("Location: {path}/"),
                        "",
                    )
                } else if path.contains("/api/files/") {
                    let tree = path
                        .split_once("/api/files/")
                        .map(|(_, rest)| rest)
                        .unwrap_or("");
                    if tree.is_empty() || tree.ends_with('/') {
                        files_resp("200 OK", "Content-Type: text/html", "<html>listing</html>")
                    } else if !tree.contains('.') {
                        // A directory requested without its trailing slash:
                        // the container 301s to the slash URL.
                        files_resp(
                            "301 Moved Permanently",
                            &format!("Location: {path}/"),
                            "",
                        )
                    } else {
                        let name = tree.rsplit('/').next().unwrap_or("");
                        files_resp(
                            "200 OK",
                            &format!("Content-Disposition: attachment; filename=\"{name}\""),
                            "file-bytes",
                        )
                    }
                } else if path.contains("/api/upload") {
                    files_resp("200 OK", "", "uploaded")
                } else if path.contains("/api/tokens") {
                    files_resp("200 OK", "", "accepted")
                } else {
                    files_resp("200 OK", "Content-Type: text/html", "<html>upstream</html>")
                };
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

/// ANT-472: the named invite (`username`) on a share link — validation
/// (existing user, not the owner), the create/list/update/revocation
/// responses, and null-to-clear on update.
#[tokio::test]
async fn test_named_invite_share_lifecycle() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;
    // The session is owned by the bootstrap admin; the invitee must be a
    // different, existing user.
    user_manager::create_user(&sc.state.db, "invitee", None).await.unwrap();
    user_manager::set_password(&sc.state.db, "invitee", "inviteepass123").await.unwrap();

    // An unknown user is rejected.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full", "username": "nobody" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "unknown user: {body:?}");
    // A self-invite is rejected.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full", "username": "admin" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "self-invite: {body:?}");

    // A valid invite is reflected in the create and list responses.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full", "username": "invitee" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let created = body.expect("created share body");
    let share_id = created.get("share_id").and_then(|v| v.as_str()).unwrap().to_string();
    assert_eq!(
        created.get("invited_username").and_then(|v| v.as_str()),
        Some("invitee")
    );
    let (status, body) = sc
        .call("GET", &format!("/api/sessions/{sid}/shares"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "list: {body:?}");
    let list_body = body.expect("list body");
    let list = list_body.as_array().unwrap();
    let row = list.iter().find(|s| s["share_id"] == share_id).unwrap();
    assert_eq!(row.get("invited_username").and_then(|v| v.as_str()), Some("invitee"));

    // A generic (unnamed) link carries no invited user.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create generic: {body:?}");
    let generic_body = body.expect("body");
    let generic_id = generic_body
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    assert_eq!(
        generic_body.get("invited_username"),
        Some(&json!(null))
    );

    // Update: set an invite, then clear it with null.
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{generic_id}"),
            Some(&json!({ "username": "invitee" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "patch set: {body:?}");
    assert_eq!(
        body.expect("body").get("invited_username").and_then(|v| v.as_str()),
        Some("invitee")
    );
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{generic_id}"),
            Some(&json!({ "username": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "patch clear: {body:?}");
    assert_eq!(body.expect("body").get("invited_username"), Some(&json!(null)));

    // Revoking an invited link answers with the link's info, naming the
    // invitee who just lost access.
    let (status, body) = sc
        .call("DELETE", &format!("/api/sessions/{sid}/shares/{share_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "revoke: {body:?}");
    let revoked = body.expect("revoked share body");
    assert_eq!(
        revoked.get("invited_username").and_then(|v| v.as_str()),
        Some("invitee")
    );
    assert_eq!(revoked.get("revoked"), Some(&json!(true)));
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

    // Revoke (200 with the revoked link's info since ANT-472, so the UI can
    // name the invitee who lost access).
    let (status, _) = sc
        .call("DELETE", &format!("/api/sessions/{sid}/shares/{share_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "revoke");
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
    assert_eq!(status, StatusCode::OK);
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

/// ANT-322 F5: the public-entry 303 carries the visitor's per-visit token,
/// so it must not be storable by a caching proxy.
#[tokio::test]
async fn test_share_redirect_no_store_cache_header() {
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

    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER, "body: {:?}", grant.body);
    assert_eq!(
        grant.header("cache-control"),
        Some("private, no-store".to_string()),
        "the 303 must carry Cache-Control: private, no-store: {:?}",
        grant.headers
    );
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
    assert_eq!(status, StatusCode::OK);
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

/// ANT-472: revoking a NAMED invite also strips the invited user's
/// participation — the participant entry in the session's viewers list and
/// the join token in the vreckan-side token map — through the same
/// `strip_share_tokens` path as a share's per-visit tokens.
#[tokio::test]
async fn test_revoke_named_invite_strips_participant() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;
    user_manager::create_user(&sc.state.db, "invitee", None).await.unwrap();
    user_manager::set_password(&sc.state.db, "invitee", "inviteepass123").await.unwrap();

    // The owner invites the user; the user joins (the grant gate passes).
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full", "username": "invitee" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let mut invitee = SecureClient::new(sc.state.clone());
    invitee.http.login("invitee", "inviteepass123").await;
    invitee.handshake().await;
    let (status, body) = invitee
        .call("POST", &format!("/api/sessions/{sid}/join"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "join: {body:?}");
    let join_token = sc
        .state
        .sessions
        .read()
        .await
        .get(&sid)
        .unwrap()
        .get("viewers")
        .and_then(|v| v.as_array())
        .and_then(|viewers| {
            viewers
                .iter()
                .find(|v| v.get("username").and_then(|u| u.as_str()) == Some("invitee"))
                .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(|t| t.to_string()))
        })
        .expect("invitee participant entry");
    assert!(
        sc.state
            .session_share_tokens
            .read()
            .await
            .get(&join_token)
            .is_some(),
            "the join token is admitted on the vreckan side"
    );

    // The owner revokes the named link.
    let (status, body) = sc
        .call("DELETE", &format!("/api/sessions/{sid}/shares/{share_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "revoke: {body:?}");

    // The invitee's participation is stripped everywhere: the viewers entry
    // is gone, and the join token no longer admits.
    let session = sc.state.sessions.read().await.get(&sid).cloned().unwrap();
    let viewers = session.get("viewers").and_then(|v| v.as_array()).unwrap();
    assert!(
        !viewers
            .iter()
            .any(|v| v.get("username").and_then(|u| u.as_str()) == Some("invitee")),
        "invitee removed from the session viewers list: {viewers:?}"
    );
    assert!(
        !sc
            .state
            .session_share_tokens
            .read()
            .await
            .values()
            .any(|r| r.share_id == format!("join:invitee")),
        "join token removed from the vreckan-side map"
    );
    let denied = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?token={join_token}"))
        .await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "the stripped join token no longer admits"
    );
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

/// ANT-472: an expired NAMED invite strips the invited user's participation
/// (viewers entry + join token) along with the link, mirroring revocation.
#[tokio::test]
async fn test_cleanup_expired_named_invite_strips_participant() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;
    user_manager::create_user(&sc.state.db, "invitee", None).await.unwrap();
    user_manager::set_password(&sc.state.db, "invitee", "inviteepass123").await.unwrap();

    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full", "username": "invitee", "expiry_hours": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let expired_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    // The invitee joins while the invite is live.
    let mut invitee = SecureClient::new(sc.state.clone());
    invitee.http.login("invitee", "inviteepass123").await;
    invitee.handshake().await;
    let (status, body) = invitee
        .call("POST", &format!("/api/sessions/{sid}/join"), Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "join: {body:?}");
    let join_token = sc
        .state
        .sessions
        .read()
        .await
        .get(&sid)
        .unwrap()
        .get("viewers")
        .and_then(|v| v.as_array())
        .and_then(|viewers| {
            viewers
                .iter()
                .find(|v| v.get("username").and_then(|u| u.as_str()) == Some("invitee"))
                .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(|t| t.to_string()))
        })
        .expect("invitee participant entry");
    assert!(
        sc.state
            .session_share_tokens
            .read()
            .await
            .get(&join_token)
            .is_some(),
            "the join token is admitted on the vreckan side before cleanup"
    );
    {
        let mut shares = sc.state.session_shares.write().await;
        if let Some(meta) = shares.get_mut(&expired_id) {
            meta.expiry_timestamp = Some(1.0);
        }
    }

    vreckan::session_shares::cleanup_expired_session_shares(&sc.state).await;

    assert!(
        sc.state.session_shares.read().await.get(&expired_id).is_none(),
        "the expired invite link is removed"
    );
    let session = sc.state.sessions.read().await.get(&sid).cloned().unwrap();
    let viewers = session.get("viewers").and_then(|v| v.as_array()).unwrap();
    assert!(
        !viewers
            .iter()
            .any(|v| v.get("username").and_then(|u| u.as_str()) == Some("invitee")),
        "the invitee's participant entry is stripped on expiry: {viewers:?}"
    );
    assert!(
        !sc
            .state
            .session_share_tokens
            .read()
            .await
            .values()
            .any(|r| r.share_id == format!("join:invitee")),
        "the join token is removed from the vreckan-side map"
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
    // The invite (ANT-472) round-trips as well.
    assert_eq!(reloaded.invited_username, None);
}

/// ANT-472: the DB JSON blob keeps its pre-invite shape — a legacy row
/// without the `invited_username` key deserializes to `None` (a generic
/// link), and a current row with the field set round-trips it.
#[tokio::test]
async fn test_session_share_legacy_row_deserializes_as_generic() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access) = seed_plain_session(&sc.state, &upstream).await;

    use sea_orm::EntityTrait;
    use vreckan::entities::session_share;
    // A legacy row (pre-invite shape: no `invited_username` key) and a
    // current row with the field set.
    let legacy_id = uuid::Uuid::new_v4().to_string();
    let legacy_data = json!({
        "session_id": sid,
        "owner_username": "admin",
        "role": "view_only",
        "caps": { "input": false, "file_browse": true, "file_download": true, "file_upload": false },
        "created_at": 1.0,
        "revoked": false,
    });
    let invited_id = uuid::Uuid::new_v4().to_string();
    let invited_data = json!({
        "session_id": sid,
        "owner_username": "admin",
        "role": "full",
        "caps": { "input": true, "file_browse": true, "file_download": true, "file_upload": true },
        "created_at": 1.0,
        "revoked": false,
        "invited_username": "invitee",
    });
    for (id, data) in [(&legacy_id, legacy_data), (&invited_id, invited_data)] {
        session_share::Entity::insert(session_share::ActiveModel {
            share_id: sea_orm::ActiveValue::Set(id.clone()),
            data: sea_orm::ActiveValue::Set(data),
        })
        .exec(&sc.state.db.conn)
        .await
        .unwrap();
    }
    sc.state.session_shares.write().await.clear();
    vreckan::session_shares::load_session_shares(&sc.state).await;
    let shares = sc.state.session_shares.read().await;
    let legacy = shares.get(&legacy_id).expect("legacy row loads");
    assert_eq!(
        legacy.invited_username, None,
        "a legacy row without the key is a generic link"
    );
    let invited = shares.get(&invited_id).expect("invited row loads");
    assert_eq!(invited.invited_username.as_deref(), Some("invitee"));
}

// ---------------------------------------------------------------------------
// WP3 (ANT-306): server-side capability enforcement in the session proxy
// ---------------------------------------------------------------------------

/// The `collab_token_{sid}` cookie header for a share visit token — what the
/// browser presents to the session proxy after the first-hit redirect.
fn collab_headers(sid: &str, token: &str) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("cookie".to_string(), format!("collab_token_{sid}={token}"));
    h
}

/// The `detail` field of a proxy 403 body.
fn proxy_detail(resp: &common::RawResponse) -> String {
    resp.json()
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn upload_body() -> Vec<u8> {
    b"filedata".to_vec()
}

/// View-only share on a promoted plain session: the preset allows browse and
/// download and blocks upload; a mid-visit cap edit leaves the in-flight
/// visit on its snapshot and cuts the next open; the owner's cookie traffic
/// is never gated.
#[tokio::test]
async fn test_share_visitor_file_caps_gate() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_files_upstream().await;
    let (sid, access) = seed_plain_session(&sc.state, &upstream).await;

    // The view-only preset: browse + download on, upload off.
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

    // Visit 1: the per-visit token admits the proxy.
    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER, "body: {:?}", grant.body);
    let t1 = location(&grant)
        .strip_prefix(&format!("/api/apps/session/{sid}/?token="))
        .and_then(|rest| rest.split('&').next())
        .expect("token in redirect: {location(&grant)}")
        .to_string();

    let h1 = collab_headers(&sid, &t1);
    // Browse (listing) and download are allowed by the preset.
    let listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &h1)
        .await;
    assert_eq!(listing.status, StatusCode::OK, "browse: {:?}", listing.body);
    let download = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/hello.txt"),
            None,
            None,
            &h1,
        )
        .await;
    assert_eq!(download.status, StatusCode::OK, "download: {:?}", download.body);
    assert!(
        download.header("content-disposition").is_some(),
        "download keeps its attachment disposition: {:?}",
        download.headers
    );
    // Upload is off in the preset: 403 before the body is charged.
    let upload = sc
        .http
        .request(
            "POST",
            &format!("/api/apps/session/{sid}/api/upload"),
            Some(upload_body()),
            Some("application/octet-stream"),
            &h1,
        )
        .await;
    assert_eq!(upload.status, StatusCode::FORBIDDEN, "upload: {:?}", upload.body);
    assert_eq!(proxy_detail(&upload), "Forbidden: File upload is disabled for this share.");
    // The no-slash directory request: the container 301s to the
    // slash-terminated listing and the proxy's upstream client follows it,
    // so the visitor receives the final listing — the browse side, allowed.
    let noslash = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/sub"),
            None,
            None,
            &h1,
        )
        .await;
    assert_eq!(
        noslash.status,
        StatusCode::OK,
        "no-slash dir resolves to the listing: {:?}",
        noslash.body
    );

    // Mid-visit cap edit: browse + download off (view_only keeps file caps).
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({ "caps": { "file_browse": false, "file_download": false } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "patch: {body:?}");

    // Next-open semantics: the in-flight visit keeps the caps it was minted
    // with (a new snapshot is taken only when the share is opened again).
    let mid_listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &h1)
        .await;
    assert_eq!(
        mid_listing.status,
        StatusCode::OK,
        "in-flight visit keeps its snapshot (browse): {:?}",
        mid_listing.body
    );
    let mid_download = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/hello.txt"),
            None,
            None,
            &h1,
        )
        .await;
    assert_eq!(
        mid_download.status,
        StatusCode::OK,
        "in-flight visit keeps its snapshot (download)"
    );

    // Visit 2: the fresh mint snapshots the edited caps — every file path
    // is now blocked.
    let grant2 = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant2.status, StatusCode::SEE_OTHER, "body: {:?}", grant2.body);
    let t2 = location(&grant2)
        .strip_prefix(&format!("/api/apps/session/{sid}/?token="))
        .and_then(|rest| rest.split('&').next())
        .expect("token in redirect: {location(&grant2)}")
        .to_string();
    let h2 = collab_headers(&sid, &t2);

    let blocked_listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &h2)
        .await;
    assert_eq!(
        blocked_listing.status,
        StatusCode::FORBIDDEN,
        "browse gate: {:?}",
        blocked_listing.body
    );
    assert_eq!(
        proxy_detail(&blocked_listing),
        "Forbidden: File browsing is disabled for this share."
    );
    // The no-slash directory resolves (via the container's 301) to the
    // slash-terminated listing — the browse side, so it is gated too.
    let blocked_noslash = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/sub"), None, None, &h2)
        .await;
    assert_eq!(
        blocked_noslash.status,
        StatusCode::FORBIDDEN,
        "no-slash dir is the browse side: {:?}",
        blocked_noslash.body
    );
    let blocked_download = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/hello.txt"),
            None,
            None,
            &h2,
        )
        .await;
    assert_eq!(
        blocked_download.status,
        StatusCode::FORBIDDEN,
        "download gate: {:?}",
        blocked_download.body
    );
    assert_eq!(
        proxy_detail(&blocked_download),
        "Forbidden: File download is disabled for this share."
    );
    let blocked_upload = sc
        .http
        .request(
            "POST",
            &format!("/api/apps/session/{sid}/api/upload"),
            Some(upload_body()),
            Some("application/octet-stream"),
            &h2,
        )
        .await;
    assert_eq!(
        blocked_upload.status,
        StatusCode::FORBIDDEN,
        "upload gate: {:?}",
        blocked_upload.body
    );
    assert_eq!(
        proxy_detail(&blocked_upload),
        "Forbidden: File upload is disabled for this share."
    );
    // Control-plane paths are not file-manager access and stay open.
    let status_path = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/status"), None, None, &h2)
        .await;
    assert_eq!(
        status_path.status,
        StatusCode::OK,
        "api/status is not a file path: {:?}",
        status_path.body
    );

    // Owner traffic (the session cookie) is never gated, even with the
    // share's file caps all off.
    let first = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?access_token={access}"))
        .await;
    assert_eq!(first.status, StatusCode::SEE_OTHER, "owner first hit: {:?}", first.body);
    let owner_cookie = format!("{}_{}", sc.state.settings.session_cookie_name, sid);
    let oh = HashMap::from([(
        "cookie".to_string(),
        format!("{owner_cookie}={access}"),
    )]);
    let owner_listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &oh)
        .await;
    assert_eq!(
        owner_listing.status,
        StatusCode::OK,
        "owner browse: {:?}",
        owner_listing.body
    );
    let owner_download = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/hello.txt"),
            None,
            None,
            &oh,
        )
        .await;
    assert_eq!(
        owner_download.status,
        StatusCode::OK,
        "owner download: {:?}",
        owner_download.body
    );
    let owner_upload = sc
        .http
        .request(
            "POST",
            &format!("/api/apps/session/{sid}/api/upload"),
            Some(upload_body()),
            Some("application/octet-stream"),
            &oh,
        )
        .await;
    assert_eq!(
        owner_upload.status,
        StatusCode::OK,
        "owner upload: {:?}",
        owner_upload.body
    );
}

/// The same gate on a room (collaboration) session: a full share passes
/// every file path; flipping the share to view-only with the file caps off
/// cuts the next open; the room's owner cookie stays ungated.
#[tokio::test]
async fn test_room_share_visitor_file_caps_gate() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_files_upstream().await;
    let (sid, access, _master) = seed_room_session(&sc.state, &upstream).await;

    // A full share: every capability on.
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

    // Visit: the room redirect carries the per-visit token.
    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER, "body: {:?}", grant.body);
    let t1 = location(&grant)
        .strip_prefix(&format!("/room/{sid}?token="))
        .expect("room redirect: {location(&grant)}")
        .to_string();

    // The full share passes all three file gates (the room's MK/slot model
    // provides the input; the file caps are independent).
    let h1 = collab_headers(&sid, &t1);
    let listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &h1)
        .await;
    assert_eq!(listing.status, StatusCode::OK, "browse: {:?}", listing.body);
    let download = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/hello.txt"),
            None,
            None,
            &h1,
        )
        .await;
    assert_eq!(download.status, StatusCode::OK, "download: {:?}", download.body);
    let upload = sc
        .http
        .request(
            "POST",
            &format!("/api/apps/session/{sid}/api/upload"),
            Some(upload_body()),
            Some("application/octet-stream"),
            &h1,
        )
        .await;
    assert_eq!(upload.status, StatusCode::OK, "upload: {:?}", upload.body);

    // The owner flips the share to view-only and switches every file cap
    // off. (A role flip alone only forces `input` off — the file caps stay
    // as configured, WP2's role-invariant semantics — so the caps are
    // named explicitly.)
    let (status, body) = sc
        .call(
            "PATCH",
            &format!("/api/sessions/{sid}/shares/{share_id}"),
            Some(&json!({
                "role": "view_only",
                "caps": {
                    "file_browse": false,
                    "file_download": false,
                    "file_upload": false
                }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "patch: {body:?}");

    // The next open is blocked on every file path.
    let grant2 = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant2.status, StatusCode::SEE_OTHER, "body: {:?}", grant2.body);
    let t2 = location(&grant2)
        .strip_prefix(&format!("/room/{sid}?token="))
        .expect("room redirect: {location(&grant2)}")
        .to_string();
    let h2 = collab_headers(&sid, &t2);
    let blocked_listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &h2)
        .await;
    assert_eq!(
        blocked_listing.status,
        StatusCode::FORBIDDEN,
        "browse gate: {:?}",
        blocked_listing.body
    );
    assert_eq!(
        proxy_detail(&blocked_listing),
        "Forbidden: File browsing is disabled for this share."
    );
    let blocked_download = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/hello.txt"),
            None,
            None,
            &h2,
        )
        .await;
    assert_eq!(
        blocked_download.status,
        StatusCode::FORBIDDEN,
        "download gate: {:?}",
        blocked_download.body
    );
    assert_eq!(
        proxy_detail(&blocked_download),
        "Forbidden: File download is disabled for this share."
    );
    let blocked_upload = sc
        .http
        .request(
            "POST",
            &format!("/api/apps/session/{sid}/api/upload"),
            Some(upload_body()),
            Some("application/octet-stream"),
            &h2,
        )
        .await;
    assert_eq!(
        blocked_upload.status,
        StatusCode::FORBIDDEN,
        "upload gate: {:?}",
        blocked_upload.body
    );
    assert_eq!(
        proxy_detail(&blocked_upload),
        "Forbidden: File upload is disabled for this share."
    );

    // The room owner's session cookie is never gated.
    let first = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?access_token={access}"))
        .await;
    assert_eq!(first.status, StatusCode::SEE_OTHER, "owner first hit: {:?}", first.body);
    let owner_cookie = format!("{}_{}", sc.state.settings.session_cookie_name, sid);
    let oh = HashMap::from([(
        "cookie".to_string(),
        format!("{owner_cookie}={access}"),
    )]);
    let owner_listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &oh)
        .await;
    assert_eq!(
        owner_listing.status,
        StatusCode::OK,
        "owner browse: {:?}",
        owner_listing.body
    );
    let owner_download = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/hello.txt"),
            None,
            None,
            &oh,
        )
        .await;
    assert_eq!(
        owner_download.status,
        StatusCode::OK,
        "owner download: {:?}",
        owner_download.body
    );
    let owner_upload = sc
        .http
        .request(
            "POST",
            &format!("/api/apps/session/{sid}/api/upload"),
            Some(upload_body()),
            Some("application/octet-stream"),
            &oh,
        )
        .await;
    assert_eq!(
        owner_upload.status,
        StatusCode::OK,
        "owner upload: {:?}",
        owner_upload.body
    );
}

/// ANT-322 F6: the no-slash `api/files` root is the browse side. A
/// browse-disabled share visitor is refused there with the browse 403
/// (previously the container's 301 to the listing passed the gate), while
/// owner traffic resolves the listing as before.
#[tokio::test]
async fn test_noslash_files_root_browse_gate() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    let (upstream, _u) = spawn_files_upstream().await;
    let (sid, access) = seed_plain_session(&sc.state, &upstream).await;

    // View-only share with browse switched off (download stays on).
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "view_only", "caps": { "file_browse": false } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Visit: the per-visit token snapshots the share's caps.
    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(grant.status, StatusCode::SEE_OTHER, "body: {:?}", grant.body);
    let t1 = location(&grant)
        .strip_prefix(&format!("/api/apps/session/{sid}/?token="))
        .and_then(|rest| rest.split('&').next())
        .expect("token in redirect: {location(&grant)}")
        .to_string();
    let h1 = collab_headers(&sid, &t1);

    // The no-slash root: the container 301s to the listing; the browse gate
    // refuses the visitor before the redirect is followed.
    let root = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files"), None, None, &h1)
        .await;
    assert_eq!(
        root.status,
        StatusCode::FORBIDDEN,
        "no-slash root is the browse side: {:?}",
        root.body
    );
    assert_eq!(
        proxy_detail(&root),
        "Forbidden: File browsing is disabled for this share."
    );
    // The slash-terminated listing is refused too (the pre-existing gate).
    let listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &h1)
        .await;
    assert_eq!(
        listing.status,
        StatusCode::FORBIDDEN,
        "listing gate: {:?}",
        listing.body
    );
    // Download is still on in this share.
    let download = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/api/files/hello.txt"),
            None,
            None,
            &h1,
        )
        .await;
    assert_eq!(download.status, StatusCode::OK, "download: {:?}", download.body);

    // Owner traffic is unaffected by the browse gate: the no-slash root
    // resolves (via the container's 301) to the listing.
    let first = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?access_token={access}"))
        .await;
    assert_eq!(first.status, StatusCode::SEE_OTHER, "owner first hit: {:?}", first.body);
    let owner_cookie = format!("{}_{}", sc.state.settings.session_cookie_name, sid);
    let oh = HashMap::from([(
        "cookie".to_string(),
        format!("{owner_cookie}={access}"),
    )]);
    let owner_root = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files"), None, None, &oh)
        .await;
    assert_eq!(
        owner_root.status,
        StatusCode::OK,
        "owner no-slash root resolves to the listing: {:?}",
        owner_root.body
    );
    let owner_listing = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/api/files/"), None, None, &oh)
        .await;
    assert_eq!(
        owner_listing.status,
        StatusCode::OK,
        "owner listing: {:?}",
        owner_listing.body
    );
}
