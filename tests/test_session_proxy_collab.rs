//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Session proxy collaboration-token acceptance (ANT-304, WP1).
//!
//! Before this change the session proxy only trusted the owner's session
//! cookie, so a collaboration-room viewer/controller whose iframe reached the
//! proxy with only a `collab_token_{id}` cookie (or a first-hit `?token=`) got
//! a 403 and a dead iframe. These tests drive the real router: the HTTP proxy
//! path (directly, via `tower::ServiceExt::oneshot`) and the WebSocket bridge
//! (against a real local socket, with a throwaway upstream that speaks
//! WebSocket and the real app router for the room WebSocket).
//!
//! They also cover the companion `catalog::stop_session` change (broadcasting
//! `session_ended` to a live collab room) and the room-mode display fix for the
//! owner re-opening a generated room link from a second browser.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use common::{build_state, SecureClient, TempDir};
use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use serde_json::json;
use vreckan::state::AppState;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsMessage = tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// Fakes + shared helpers
// ---------------------------------------------------------------------------

/// A throwaway HTTP server that answers every request with a small 200 body.
/// Stands in for the app container the session proxy reverse-proxies to.
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

/// A throwaway app-container upstream that speaks WebSocket: on connect it
/// sends one known text frame and then stays open (draining the client).
/// Served on a real local port so the session proxy's `tokio_tungstenite`
/// client can complete the handshake and relay frames.
async fn spawn_ws_upstream(frame: &str) -> (String, tokio::task::JoinHandle<()>) {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::routing::get;
    use axum::Router;

    let app = Router::new().route(
        "/{*path}",
        get({
            let frame = frame.to_string();
            move |up: WebSocketUpgrade| {
                let frame = frame.clone();
                async move {
                    up.on_upgrade(move |socket: WebSocket| async move {
                        let (mut tx, mut rx) = socket.split();
                        let _ = tx.send(Message::Text(frame.into())).await;
                        while rx.next().await.is_some() {}
                    })
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// Serve the real app router on a throwaway local TCP socket (the
/// `tower::oneshot` harness has no socket, which the WebSocket bridge needs).
async fn serve_app(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = vreckan::app::router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// Seed a collaboration session record (the shape `_launch_common` stores) with
/// a controller token and a viewers list, and (optionally) point it at an
/// upstream host.
async fn seed_collab_session(
    state: &AppState,
    upstream: Option<&str>,
    viewers: Vec<serde_json::Value>,
) -> (String, String, String) {
    let (ip, port) = match upstream {
        Some(h) => {
            let (ip, port) = h.rsplit_once(':').unwrap();
            (json!(ip), port.parse::<i64>().unwrap())
        }
        None => (json!(null), 0),
    };
    let access_token = format!("access-{}", uuid::Uuid::new_v4());
    let controller_token = format!("ctrl-{}", uuid::Uuid::new_v4());
    let sid = uuid::Uuid::new_v4().to_string();
    let mut session = json!({
        "instance_id": "ctr-1",
        "ip": ip,
        "port": port,
        "created_at": 1.0,
        "access_token": access_token,
        "controller_token": controller_token,
        "is_collaboration": true,
        "provider_app_id": "ghost-app",
        "username": "admin",
        "app_name": "Test App",
        "app_logo": "x",
        "viewers": viewers,
        "container_registry": {},
    });
    if upstream.is_none() {
        session.as_object_mut().unwrap().remove("ip");
        session.as_object_mut().unwrap().remove("port");
    }
    state.sessions.write().await.insert(sid.clone(), session);
    (sid, access_token, controller_token)
}

/// Connect a WebSocket client to a URL, sending an optional cookie. The RFC
/// 6455 handshake headers are set explicitly, matching the browser client (and
/// the session proxy's own upstream handshake).
async fn ws_connect(url: &str, cookie: Option<&str>) -> WsStream {
    let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
    // tungstenite requires an explicit `Host` header (like session_proxy's
    // upstream handshake).
    let host = url
        .strip_prefix("ws://")
        .and_then(|rest| rest.split_once('/'))
        .map(|(h, _)| h)
        .unwrap_or("127.0.0.1");
    let mut builder = http::Request::builder()
        .method("GET")
        .uri(url)
        .header("host", host)
        .header("sec-websocket-key", &key)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13");
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    let request = builder.body(()).expect("build ws request");
    let (stream, _resp) =
        tokio_tungstenite::connect_async(request).await.expect("ws connect succeeded");
    stream
}

/// Drain pending frames until the stream is quiet for `idle` (consumes the
/// join/state broadcasts a fresh room connection triggers).
async fn read_until_idle(stream: &mut WsStream, idle: Duration) {
    loop {
        match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        }
    }
}

/// Assert that no binary frame arrives within `window` (the readonly input
/// drop). Text/ping/pong frames are drained.
async fn expect_no_binary(stream: &mut WsStream, window: Duration) {
    loop {
        match tokio::time::timeout(window, stream.next()).await {
            Ok(Some(Ok(m))) => match m {
                WsMessage::Binary(_) => panic!("unexpected binary frame (input should be dropped)"),
                _ => continue,
            },
            Ok(Some(Err(e))) => panic!("stream error while waiting: {e}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

/// Read until a binary frame arrives within `window` and return it.
async fn expect_binary(stream: &mut WsStream, window: Duration) -> WsMessage {
    loop {
        match tokio::time::timeout(window, stream.next()).await {
            Ok(Some(Ok(m))) => match m {
                WsMessage::Binary(_) => return m,
                _ => continue,
            },
            Ok(Some(Err(e))) => panic!("stream error while waiting: {e}"),
            Ok(None) => panic!("stream closed before a binary frame arrived"),
            Err(_) => panic!("timed out waiting for a binary frame"),
        }
    }
}

/// Read the first text or binary frame within `window`.
async fn expect_text_or_binary(stream: &mut WsStream, window: Duration) -> Option<WsMessage> {
    loop {
        match tokio::time::timeout(window, stream.next()).await {
            Ok(Some(Ok(m))) => match m {
                WsMessage::Text(_) | WsMessage::Binary(_) => return Some(m),
                _ => continue,
            },
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP proxy: collab token acceptance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_http_viewer_collab_cookie_is_proxied() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access, _ctrl) = seed_collab_session(
        &sc.state,
        Some(&upstream),
        vec![json!({
            "token": "viewer-1",
            "permission": "participant",
            "username": "V",
            "slot": null,
            "mk_control": false
        })],
    )
    .await;

    let cookie_name = format!("collab_token_{sid}");
    let mut headers = HashMap::new();
    headers.insert("cookie".to_string(), format!("{cookie_name}=viewer-1"));
    // The room iframe loads `?token=...`; the collab cookie is what the proxy
    // now accepts. Must be proxied (200), not 403.
    let resp = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/?token=viewer-1"),
            None,
            None,
            &headers,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "body: {:?}",
        String::from_utf8_lossy(&resp.body)
    );
}

#[tokio::test]
async fn test_http_viewer_first_hit_token_is_proxied() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access, _ctrl) = seed_collab_session(
        &sc.state,
        Some(&upstream),
        vec![json!({
            "token": "viewer-1",
            "permission": "participant",
            "username": "V",
            "slot": null,
            "mk_control": false
        })],
    )
    .await;

    // No collab cookie (a fresh tab that opened the iframe URL directly): the
    // first-hit `?token=` is accepted for a known viewer.
    let resp = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?token=viewer-1"))
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "body: {:?}",
        String::from_utf8_lossy(&resp.body)
    );
}

#[tokio::test]
async fn test_http_owner_second_browser_via_room_link_is_proxied() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, access_token, controller_token) =
        seed_collab_session(&sc.state, Some(&upstream), Vec::new()).await;

    // The owner opens the generated room link in a *second* browser: no owner
    // session cookie, just the access token + controller token from the link.
    let room = sc
        .http
        .get(&format!(
            "/room/{sid}?access_token={access_token}&token={controller_token}"
        ))
        .await;
    assert_eq!(
        room.status,
        StatusCode::OK,
        "body: {:?}",
        String::from_utf8_lossy(&room.body)
    );

    // The room page sets the `collab_token_{sid}` cookie to the controller
    // token (alongside the owner session cookie); the iframe then loads the
    // session proxy with that cookie and no owner session cookie.
    let collab_cookie = format!("collab_token_{sid}=");
    let cookie_value = room
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            v.strip_prefix(&collab_cookie)
                .map(|rest| rest.split(';').next().unwrap_or("").to_string())
        })
        .expect("room page sets a collab token cookie");
    assert_eq!(cookie_value, controller_token);

    let mut headers = HashMap::new();
    headers.insert(
        "cookie".to_string(),
        format!("collab_token_{sid}={cookie_value}"),
    );
    let resp = sc
        .http
        .request(
            "GET",
            &format!("/api/apps/session/{sid}/?token={cookie_value}"),
            None,
            None,
            &headers,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "body: {:?}",
        String::from_utf8_lossy(&resp.body)
    );
}

#[tokio::test]
async fn test_http_bad_collab_token_is_forbidden() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, _u) = spawn_http_upstream().await;
    let (sid, _access, _ctrl) = seed_collab_session(
        &sc.state,
        Some(&upstream),
        vec![json!({
            "token": "viewer-1",
            "permission": "participant",
            "username": "V",
            "slot": null,
            "mk_control": false
        })],
    )
    .await;

    let mut headers = HashMap::new();
    headers.insert(
        "cookie".to_string(),
        format!("collab_token_{sid}=not-a-real-token"),
    );
    let resp = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/"), None, None, &headers)
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_http_noncollab_session_ignores_collab_token() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, _u) = spawn_http_upstream().await;
    let (up_ip, up_port) = upstream.rsplit_once(':').unwrap();
    // A plain (non-collaboration) session: a collab-style token must not grant
    // access, and the owner's cookie still works.
    let access_token = format!("access-{}", uuid::Uuid::new_v4());
    let sid = uuid::Uuid::new_v4().to_string();
    let session = json!({
        "instance_id": "ctr-1",
        "ip": up_ip,
        "port": up_port.parse::<i64>().unwrap(),
        "access_token": access_token,
        "is_collaboration": false,
        "controller_token": "ctrl-x",
        "viewers": [{ "token": "viewer-1", "permission": "participant" }],
        "username": "admin",
        "container_registry": {},
    });
    sc.state.sessions.write().await.insert(sid.clone(), session);

    // A collab viewer token does not work on a plain session.
    let mut headers = HashMap::new();
    headers.insert("cookie".to_string(), format!("collab_token_{sid}=viewer-1"));
    let resp = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/"), None, None, &headers)
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);

    // ...but the owner's session cookie still proxies (behavior preserved).
    let owner_cookie = format!("{}_{}", sc.state.settings.session_cookie_name, sid);
    let mut ok_headers = HashMap::new();
    ok_headers.insert(
        "cookie".to_string(),
        format!("{owner_cookie}={access_token}"),
    );
    let resp = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/"), None, None, &ok_headers)
        .await;
    assert_eq!(resp.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// WebSocket bridge: collab token acceptance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ws_viewer_collab_token_is_relayed() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, _u) = spawn_ws_upstream("UPSTREAM_HELLO").await;
    let (sid, _access, _ctrl) = seed_collab_session(
        &sc.state,
        Some(&upstream),
        vec![json!({
            "token": "viewer-1",
            "permission": "participant",
            "username": "V",
            "slot": null,
            "mk_control": false
        })],
    )
    .await;
    let (host_port, _server) = serve_app(sc.state.clone()).await;

    let cookie = format!("collab_token_{sid}=viewer-1");
    let url = format!("ws://{host_port}/api/apps/session/{sid}/ws");
    let mut stream = ws_connect(&url, Some(&cookie)).await;

    // A valid viewer collab token establishes the bridge: the upstream's frame
    // is relayed back to the client.
    let frame = expect_text_or_binary(&mut stream, Duration::from_secs(3))
        .await
        .expect("viewer collab token should be relayed");
    match frame {
        WsMessage::Text(t) => assert!(
            t.contains("UPSTREAM_HELLO"),
            "expected the upstream frame, got: {t}"
        ),
        other => panic!("expected the upstream text frame, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_ws_bad_collab_token_is_closed() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, _u) = spawn_ws_upstream("UPSTREAM_HELLO").await;
    let (sid, _access, _ctrl) = seed_collab_session(
        &sc.state,
        Some(&upstream),
        vec![json!({
            "token": "viewer-1",
            "permission": "participant",
            "username": "V",
            "slot": null,
            "mk_control": false
        })],
    )
    .await;
    let (host_port, _server) = serve_app(sc.state.clone()).await;

    let cookie = format!("collab_token_{sid}=not-a-real-token");
    let url = format!("ws://{host_port}/api/apps/session/{sid}/ws");
    let mut stream = ws_connect(&url, Some(&cookie)).await;

    // An invalid collab token upgrades (101) but immediately closes the client
    // side — no upstream frame is ever relayed.
    let mut saw_close = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(3), stream.next()).await {
            Ok(Some(Ok(WsMessage::Close(_)))) => {
                saw_close = true;
                break;
            }
            Ok(Some(Ok(m))) => {
                panic!("unexpected frame for a bad collab token: {m:?}")
            }
            Ok(Some(Err(e))) => panic!("stream error: {e}"),
            Ok(None) => break,
            Err(_) => panic!("timed out waiting for the close of a bad collab token"),
        }
    }
    assert!(
        saw_close,
        "bad collab token should close the client WebSocket"
    );
}

// ---------------------------------------------------------------------------
// Room WebSocket: readonly input dropped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_room_ws_readonly_input_dropped() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (sid, _access, _ctrl) = seed_collab_session(
        &sc.state,
        None,
        vec![
            json!({
                "token": "ro-1",
                "permission": "readonly",
                "username": "RO",
                "slot": null,
                "mk_control": false
            }),
            json!({
                "token": "vw-1",
                "permission": "participant",
                "username": "VW",
                "slot": null,
                "mk_control": false
            }),
        ],
    )
    .await;
    let (host_port, _server) = serve_app(sc.state.clone()).await;

    let ro_url = format!("ws://{host_port}/ws/room/{sid}?token=ro-1");
    let vw_url = format!("ws://{host_port}/ws/room/{sid}?token=vw-1");
    let mut ro = ws_connect(&ro_url, None).await;
    let mut vw = ws_connect(&vw_url, None).await;

    // Consume the join/state broadcasts both connections trigger.
    read_until_idle(&mut ro, Duration::from_millis(200)).await;
    read_until_idle(&mut vw, Duration::from_millis(200)).await;

    // A readonly viewer's binary (input) packet is dropped server-side, so it
    // never reaches the other connection.
    ro.send(WsMessage::Binary(vec![1, 2, 3, 4].into()))
        .await
        .expect("ro sends input");
    expect_no_binary(&mut vw, Duration::from_millis(500)).await;

    // A participant's binary packet is broadcast to the other connection.
    vw.send(WsMessage::Binary(vec![9, 8, 7, 6].into()))
        .await
        .expect("vw sends input");
    let relayed = expect_binary(&mut ro, Duration::from_secs(3)).await;
    assert_eq!(
        relayed.into_data().to_vec(),
        vec![9, 8, 7, 6],
        "participant input should be relayed to the readonly viewer"
    );
}

// ---------------------------------------------------------------------------
// stop_session: session_ended broadcast to a live collab room
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stop_session_broadcasts_session_ended_to_room() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (sid, _access, controller_token) =
        seed_collab_session(&sc.state, None, Vec::new()).await;
    let (host_port, _server) = serve_app(sc.state.clone()).await;

    // A live controller in the room (the session must exist and be a collab
    // session for the room WS to accept the connection).
    let url = format!("ws://{host_port}/ws/room/{sid}?token={controller_token}");
    let mut stream = ws_connect(&url, None).await;
    read_until_idle(&mut stream, Duration::from_millis(200)).await;

    // Stopping the session must reach the live room as a `session_ended`
    // message (not a dead iframe).
    vreckan::catalog::stop_session(&sc.state, &sid).await;

    let mut saw_session_ended = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(3), stream.next()).await {
            Ok(Some(Ok(WsMessage::Text(t)))) => {
                if t.contains("\"session_ended\"") {
                    saw_session_ended = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("stream error: {e}"),
            Ok(None) => break,
            Err(_) => panic!("timed out waiting for the session_ended broadcast"),
        }
    }
    assert!(saw_session_ended, "stop_session should broadcast session_ended");
}
