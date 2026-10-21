//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Selkies 2.0.0 MK hand-off (ANT-443).
//!
//! When a second controller joins a running plain session over the default
//! WebSockets transport, selkies kills the previous primary but keeps
//! `active_mk_token` on the killed owner's token (it is only re-derived from
//! a token-table push, never from a client disconnect). The surviving
//! joiner therefore sat at `MK_ACCESS=false` until a table change. The
//! vreckan-side fix tracks data-plane connections per session and re-pushes
//! the plain-session token table on every connect/disconnect, granting
//! `mk_control` to the most recently connected controller — the
//! newest-controller-wins hand-off, without a client refresh.
//!
//! These tests drive the real router with a fake app container that records
//! both the WebSocket connects (token, in order) and the token-table POSTs
//! it receives, and assert the table flips with the connect order.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{build_state, SecureClient, TempDir};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};

#[derive(Clone)]
struct UpstreamCapture {
    /// Token-table POST bodies, in receive order.
    tables: Arc<Mutex<Vec<Value>>>,
    /// Data-plane WS connects (the token each presented), in connect order.
    connects: Arc<Mutex<Vec<String>>>,
}

/// A fake app container: upgrades any GET to a WebSocket (recording the
/// presented token, then draining) and records every token-table POST body.
async fn spawn_capture_upstream() -> (String, UpstreamCapture) {
    use axum::extract::ws::{WebSocketUpgrade, WebSocket};
    use axum::routing::get;
    use axum::{Json, Router};

    let cap = UpstreamCapture {
        tables: Arc::new(Mutex::new(Vec::new())),
        connects: Arc::new(Mutex::new(Vec::new())),
    };

    let app = Router::new()
        .route(
            "/api/apps/session/{*path}",
            get({
                let cap = cap.clone();
                move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>, up: WebSocketUpgrade| {
                    let cap = cap.clone();
                    async move {
                        let token = q.get("token").cloned().unwrap_or_default();
                        up.on_upgrade(move |socket: WebSocket| async move {
                            {
                                let mut conns = cap.connects.lock().await;
                                if !token.is_empty() {
                                    conns.push(token);
                                }
                            }
                            let (mut tx, mut rx) = socket.split();
                            let _ = tx
                                .send(axum::extract::ws::Message::Text("UP".into()))
                                .await;
                            while rx.next().await.is_some() {}
                        })
                    }
                }
            })
            .post({
                let cap = cap.clone();
                move |Json(body): Json<Value>| {
                    let cap = cap.clone();
                    async move {
                        cap.tables.lock().await.push(body);
                    }
                }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("127.0.0.1:{}", addr.port()), cap)
}

/// Serve the real app router on a throwaway local TCP socket (the WebSocket
/// bridge needs a real socket).
async fn serve_app(state: vreckan::state::AppState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = vreckan::app::router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// Connect a WebSocket client to a URL with an optional cookie, mirroring
/// the session proxy test harness.
async fn ws_connect(url: &str, cookie: Option<&str>) -> WsStream {
    let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
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
    let (stream, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect succeeded");
    stream
}

/// Poll until `tables` holds at least `count` pushes and the most recent one
/// matches the expectation, or fail after `timeout`.
async fn expect_last_table(
    cap: &UpstreamCapture,
    timeout: Duration,
    expect_owner_mk: bool,
    expect_joiner_mk: bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let tables = cap.tables.lock().await.clone();
        if let Some(last) = tables.last() {
            let owner = last
                .get("at-1")
                .and_then(|e| e.get("mk_control"))
                .and_then(|v| v.as_bool());
            let joiner = last
                .get("vb-1")
                .and_then(|e| e.get("mk_control"))
                .and_then(|v| v.as_bool());
            if owner == Some(expect_owner_mk) && joiner == Some(expect_joiner_mk) {
                return last.clone();
            }
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for the expected token table; last: {:?}",
                tables.last()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A plain (non-collab) session in selkies secure mode: owner addressed by
/// the access token, one controller-role joiner in the viewers list.
async fn seed_plain_session(state: &vreckan::state::AppState, upstream: &str) -> String {
    let (ip, port) = upstream.rsplit_once(':').unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let session = json!({
        "instance_id": "ctr-1",
        "ip": ip,
        "port": port.parse::<i64>().unwrap(),
        "created_at": 1.0,
        "access_token": "at-1",
        "master_token": "mt-1",
        "is_collaboration": false,
        "provider_app_id": "ghost-app",
        "username": "admin",
        "app_name": "Test App",
        "app_logo": "x",
        "viewers": [{
            "token": "vb-1",
            "role": "controller",
            "permission": "participant",
            "username": "bob",
            "slot": null,
            "mk_control": false
        }],
        "container_registry": {},
    });
    // Admit the joiner's per-visit token on the vreckan side too (the share
    // grant records it; the proxy admits it for the data-plane bridge).
    state
        .session_share_tokens
        .write()
        .await
        .insert(
            "vb-1".to_string(),
            vreckan::session_shares::SessionShareTokenRecord {
                share_id: "join:bob".to_string(),
                session_id: sid.clone(),
                caps: vreckan::session_shares::SessionShareCaps {
                    input: true,
                    file_browse: true,
                    file_download: true,
                    file_upload: true,
                },
                created_at: 0.0,
                expiry: None,
            },
        );
    state.sessions.write().await.insert(sid.clone(), session);
    sid
}

#[tokio::test]
async fn test_mk_follows_newest_connected_controller() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, cap) = spawn_capture_upstream().await;
    let sid = seed_plain_session(&sc.state, &upstream).await;
    let (host_port, _server) = serve_app(sc.state.clone()).await;

    let data_ws = format!("ws://{host_port}/api/apps/session/{sid}/api/websockets");

    // 1. The owner connects: the most recent controller is the owner, so the
    //    refreshed table grants him mk (the joiner keeps the static false).
    let owner_cookie = format!(
        "{}_{sid}=at-1",
        sc.state.settings.session_cookie_name
    );
    let mut owner = ws_connect(&data_ws, Some(&owner_cookie)).await;
    let _ = expect_last_table(&cap, Duration::from_secs(5), true, false).await;
    // Settle: no further push for an unchanged derivation.
    let count = cap.tables.lock().await.len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        cap.tables.lock().await.len(),
        count,
        "an unchanged derivation must not re-push"
    );

    // 2. The joiner connects: he is now the most recent controller. The
    //    table flips so the survivor of the hand-off holds mk — the fix
    //    (before it the joiner stayed at mk_control=false and, selkies-side,
    //    at MK_ACCESS=false until a refresh).
    let mut joiner = ws_connect(&format!("{data_ws}?token=vb-1"), None).await;
    let _ = expect_last_table(&cap, Duration::from_secs(5), false, true).await;
    assert_eq!(
        *cap.connects.lock().await,
        vec!["at-1".to_string(), "vb-1".to_string()],
        "connect order must be owner then joiner"
    );

    // 3. The joiner leaves (client closes): the owner is the most recent
    //    connected controller again — mk returns to him.
    joiner.close(None).await.ok();
    let _ = expect_last_table(&cap, Duration::from_secs(5), true, false).await;

    // 4. The joiner re-enters (the "refresh" case): newest wins again, no
    //    further client action needed after this connect.
    joiner = ws_connect(&format!("{data_ws}?token=vb-1"), None).await;
    let _ = expect_last_table(&cap, Duration::from_secs(5), false, true).await;

    // 5. The owner's client is killed (his leg closes; selkies does this on
    //    the hand-off): the joiner stays the most recent connected
    //    controller, so the table must NOT flip back — that is the stale
    //    mk-owner the bug was about.
    owner.close(None).await.ok();
    let _ = expect_last_table(&cap, Duration::from_secs(5), false, true).await;
    let count = cap.tables.lock().await.len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        cap.tables.lock().await.len(),
        count,
        "the killed owner's disconnect must not re-grant him mk"
    );
    joiner.close(None).await.ok();
}

#[tokio::test]
async fn test_viewers_do_not_steal_mk() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let (upstream, cap) = spawn_capture_upstream().await;
    let sid = seed_plain_session(&sc.state, &upstream).await;
    // A read-only viewer (no controller role) alongside the joiner.
    {
        let mut session = sc
            .state
            .sessions
            .read()
            .await
            .get(&sid)
            .cloned()
            .unwrap();
        let viewers = session.get_mut("viewers").and_then(|v| v.as_array_mut()).unwrap();
        viewers.push(json!({
            "token": "vr-1",
            "role": "viewer",
            "permission": "readonly",
            "username": "carol",
            "slot": null,
            "mk_control": false
        }));
        sc.state.sessions.write().await.insert(sid.clone(), session);
        sc.state
            .session_share_tokens
            .write()
            .await
            .insert(
                "vr-1".to_string(),
                vreckan::session_shares::SessionShareTokenRecord {
                    share_id: "share:carol".to_string(),
                    session_id: sid.clone(),
                    caps: vreckan::session_shares::SessionShareCaps {
                        input: false,
                        file_browse: true,
                        file_download: true,
                        file_upload: false,
                    },
                    created_at: 0.0,
                    expiry: None,
                },
            );
    }
    let (host_port, _server) = serve_app(sc.state.clone()).await;

    let data_ws = format!("ws://{host_port}/api/apps/session/{sid}/api/websockets");
    let owner_cookie = format!("{}_{sid}=at-1", sc.state.settings.session_cookie_name);
    let _owner = ws_connect(&data_ws, Some(&owner_cookie)).await;
    let _ = expect_last_table(&cap, Duration::from_secs(5), true, false).await;

    let _joiner = ws_connect(&format!("{data_ws}?token=vb-1"), None).await;
    let _ = expect_last_table(&cap, Duration::from_secs(5), false, true).await;

    // The readonly viewer connects LAST: viewers never claim the primary
    // display, so the joiner (the newest connected controller) keeps mk.
    let _viewer = ws_connect(&format!("{data_ws}?token=vr-1"), None).await;
    let _ = expect_last_table(&cap, Duration::from_secs(5), false, true).await;
    assert_eq!(
        *cap.connects.lock().await,
        vec![
            "at-1".to_string(),
            "vb-1".to_string(),
            "vr-1".to_string()
        ],
        "connect order must be owner, joiner, viewer"
    );
}
