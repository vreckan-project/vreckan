//! ANT-450: the pinned legacy (guacamole-lite) image, served through the
//! session proxy.
//!
//! F-P5: the legacy gclient serves its page at the in-container `/` and
//! references assets root-absolute (`/public/...`, the `/guaclite` tunnel).
//! The proxy must (a) remap the upstream leg to the in-container paths and
//! (b) rewrite those root-absolute references to session-prefixed paths so
//! they resolve through the proxy (with the token gate).
//!
//! F-P6: the tunnel dial ends up at the session-prefixed path, so the
//! desktop data plane is token-gated by the proxy.
//!
//! Like the collab tests, the upstreams run on real local sockets (the proxy
//! dials them with its own hyper/tungstenite clients), while the app router
//! is driven through `TestClient` (tower oneshot) except for the WebSocket
//! bridge test, which needs a real socket and uses a served router plus a
//! `tokio_tungstenite` client.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use http::{Request, StatusCode};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

mod common;

use common::{build_state, TempDir, TestClient};
use vreckan::state::AppState;

/// In-container gclient stand-in: serves the page at `/` with the same
/// root-absolute asset references as the real gclient, statics at their
/// in-container paths, and embeds the received request path in every body
/// so a test can verify what the proxy dialed upstream.
async fn spawn_legacy_upstream() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            tokio::spawn(async move {
                // Read until the header block ends; a single read can land a
                // partial first line on slow sockets, which would otherwise
                // lose the request path.
                let mut raw: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = match sock.read(&mut chunk).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    raw.extend_from_slice(&chunk[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") || raw.len() > 65536 {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&raw);
                // The request target without its query, if any.
                let path = match req.split_whitespace().nth(1) {
                    Some(p) => p.split_once('?').map(|(p, _)| p).unwrap_or(p).to_string(),
                    None => "/".to_string(),
                };
                // The page route: the legacy gclient serves it at `/`; the
                // selkies (secure) container serves it prefix-intact at
                // `/api/apps/session/{sid}/`. Both arrive here as the
                // session's entry path.
                let is_page =
                    path == "/" || (path.starts_with("/api/apps/session/") && path.ends_with('/'));
                let (status, ctype, body) = if is_page {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        format!(
                            "<html><head><link href=\"/public/css/vdi.css\" rel=\"stylesheet\"></head>\
                             <body><script src=\"/public/vendor/guac/js/guac.min.js\"></script>\
                             <script src=\"/public/js/rdp.js\"></script>\
                             <!--UPSTREAM_PATH:{path}--></body></html>"
                        ),
                    )
                } else if path.starts_with("/public/") {
                    (
                        "200 OK",
                        "application/javascript",
                        if path == "/public/js/rdp.js" {
                            format!(
                                "var tunnel = new Guacamole.WebSocketTunnel(wsprotocol + '//' + \
                                 host + ':' + port + '/guaclite?token=' + cs); \
                                 /*UPSTREAM_PATH:{path}*/"
                            )
                        } else {
                            format!("/*UPSTREAM_PATH:{path}*/")
                        },
                    )
                } else if path == "/guaclite" {
                    (
                        "200 OK",
                        "text/plain",
                        format!("/*WS_UPSTREAM_PATH:{path}*/"),
                    )
                } else {
                    (
                        "404 Not Found",
                        "text/plain",
                        format!("not found: {path}"),
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// Upstream that speaks WebSocket like the gclient: on connect it sends one
/// text frame naming the path it received, so the bridge test can assert
/// what the proxy dialed.
async fn spawn_guaclite_ws_upstream() -> (String, tokio::task::JoinHandle<()>) {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::extract::Path;
    use axum::routing::get;
    use axum::Router;

    let app = Router::new().route(
        "/{*path}",
        get(move |Path(path): Path<String>, up: WebSocketUpgrade| {
            async move {
                up.on_upgrade(move |socket: WebSocket| async move {
                    let (mut tx, mut rx) = socket.split();
                    let _ = tx
                        .send(Message::Text(format!("WS_UPSTREAM_PATH:{path}").into()))
                        .await;
                    while rx.next().await.is_some() {}
                })
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// Serve the real app router on a throwaway socket (needed by the WebSocket
/// bridge test; the tower oneshot harness has no socket).
async fn serve_app(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = vreckan::app::router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// The per-session cookie name, mirroring `session_cookie_name` in the proxy
/// (the settings default is `vreckan_session_token`).
fn session_cookie_name(state: &AppState, session_id: &str) -> String {
    format!("{}_{}", state.settings.session_cookie_name, session_id)
}

async fn seed_session(
    state: &AppState,
    upstream: &str,
    capability: &str,
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
        "controller_token": format!("ctrl-{}", uuid::Uuid::new_v4()),
        "selkies_capability": capability,
        "is_collaboration": false,
        "provider_app_id": "ghost-app",
        "username": "admin",
        "app_name": "Test App",
        "app_logo": "x",
        "viewers": [],
        "container_registry": {},
    });
    state.sessions.write().await.insert(sid.clone(), session);
    (sid, access_token)
}

/// First hit on the session entry URL: 303 to the canonical path plus the
/// session cookie (unchanged by this fix — the proxy entry flow).
#[tokio::test]
async fn legacy_first_hit_sets_cookie_and_redirects() {
    let _root = TempDir::new();
    let (upstream, _up) = spawn_legacy_upstream().await;
    let state = build_state(_root.path()).await;
    let (sid, access_token) = seed_session(&state, &upstream, "legacy").await;
    let cookie_name = session_cookie_name(&state, &sid);
    let client = TestClient::new(state);

    let mut headers = HashMap::new();
    headers.insert("host".to_string(), "127.0.0.1".to_string());
    let resp = client
        .get_with(
            &format!("/api/apps/session/{sid}/?access_token={access_token}"),
            &headers,
        )
        .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER, "body: {:?}", resp.body);
    let set_cookie = resp.header("set-cookie").unwrap_or_default();
    assert!(
        set_cookie.contains(&format!("{cookie_name}={access_token}")),
        "{set_cookie}"
    );
    assert_eq!(
        resp.header("location").as_deref(),
        Some(format!("https://127.0.0.1/api/apps/session/{sid}/").as_str()),
        "Location must be the session's canonical entry path"
    );
}

/// F-P5: the gclient page is served from the in-container `/` and its
/// root-absolute asset references come back session-prefixed, so the
/// browser resolves them through the proxy instead of the vreckan root.
#[tokio::test]
async fn legacy_page_served_from_in_container_root_with_prefixed_refs() {
    let _root = TempDir::new();
    let (upstream, _up) = spawn_legacy_upstream().await;
    let state = build_state(_root.path()).await;
    let (sid, access_token) = seed_session(&state, &upstream, "legacy").await;
    let cookie_name = session_cookie_name(&state, &sid);
    let client = TestClient::new(state);

    let mut headers = HashMap::new();
    headers.insert(
        "cookie".to_string(),
        format!("{cookie_name}={access_token}"),
    );
    let resp = client
        .get_with(&format!("/api/apps/session/{sid}/"), &headers)
        .await;
    assert_eq!(resp.status, StatusCode::OK, "body: {:?}", resp.body);
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    // The proxy dialed the in-container root.
    assert!(body.contains("<!--UPSTREAM_PATH:/-->"), "{body}");
    // The root-absolute references were rewritten to the session's public
    // proxy prefix.
    assert!(
        body.contains(&format!(r#"href="/api/apps/session/{sid}/public/css/vdi.css""#)),
        "{body}"
    );
    assert!(
        body.contains(
            &format!(r#"src="/api/apps/session/{sid}/public/vendor/guac/js/guac.min.js""#)
        ),
        "{body}"
    );
    assert!(
        body.contains(&format!(r#"src="/api/apps/session/{sid}/public/js/rdp.js""#)),
        "{body}"
    );
    assert!(!body.contains(r#"="/public/"#), "{body}");
}

/// F-P5/F-P6: the rdp.js tunnel dial is rewritten to the session-prefixed
/// `/guaclite`, so the RDP data plane crosses the proxy's token gate, and
/// the rewritten body's content-length matches the post-rewrite body.
#[tokio::test]
async fn legacy_rdp_js_tunnel_ref_prefixed() {
    let _root = TempDir::new();
    let (upstream, _up) = spawn_legacy_upstream().await;
    let state = build_state(_root.path()).await;
    let (sid, access_token) = seed_session(&state, &upstream, "legacy").await;
    let cookie_name = session_cookie_name(&state, &sid);
    let client = TestClient::new(state);

    let mut headers = HashMap::new();
    headers.insert(
        "cookie".to_string(),
        format!("{cookie_name}={access_token}"),
    );
    let resp = client
        .get_with(
            &format!("/api/apps/session/{sid}/public/js/rdp.js"),
            &headers,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "body: {:?}", resp.body);
    let len = resp
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok());
    // The rewritten body's content-length is the post-rewrite length.
    assert_eq!(len, Some(resp.body.len()), "content-length must match the rewritten body");
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    // The upstream leg was remapped to the in-container script path.
    assert!(body.contains("/*UPSTREAM_PATH:/public/js/rdp.js*/"), "{body}");
    // The tunnel dial is session-prefixed so it reaches the proxy (and the
    // proxy's token gate) instead of the vreckan root — F-P6: the desktop
    // data plane is now token-gated.
    assert!(
        body.contains(&format!("'/api/apps/session/{sid}/guaclite?token='")),
        "{body}"
    );
}

/// Auth still applies to the remapped page leg: no cookie → 403.
#[tokio::test]
async fn legacy_page_forbidden_without_cookie() {
    let _root = TempDir::new();
    let (upstream, _up) = spawn_legacy_upstream().await;
    let state = build_state(_root.path()).await;
    let (sid, _access_token) = seed_session(&state, &upstream, "legacy").await;
    let client = TestClient::new(state);

    let resp = client.get(&format!("/api/apps/session/{sid}/")).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "body: {:?}", resp.body);
}

/// F-P5: the WebSocket bridge dials the in-container tunnel path for legacy
/// sessions. The client connects to the session-prefixed URL with the
/// session cookie; the marker frame the fake gclient sends names the path
/// the proxy dialed.
#[tokio::test]
async fn legacy_guaclite_tunnel_reaches_in_container_path() {
    let _root = TempDir::new();
    let (upstream, _up) = spawn_guaclite_ws_upstream().await;
    let state = build_state(_root.path()).await;
    let (sid, access_token) = seed_session(&state, &upstream, "legacy").await;
    let cookie_name = session_cookie_name(&state, &sid);
    let (addr, _app) = serve_app(state).await;

    let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
    let request = Request::builder()
        .method("GET")
        .uri(format!("ws://{addr}/api/apps/session/{sid}/guaclite?token=connstring"))
        .header("host", addr.as_str())
        .header("cookie", &format!("{cookie_name}={access_token}"))
        .header("sec-websocket-key", &key)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .body(())
        .expect("build ws request");
    let (mut stream, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect succeeded");

    let marker = first_text(&mut stream, Duration::from_secs(5)).await;
    // The fake upstream is an axum `/{*path}` wildcard: the capture carries
    // the tunnel path without its leading slash.
    assert!(
        marker.as_deref() == Some("WS_UPSTREAM_PATH:guaclite"),
        "proxy dialed the in-container tunnel path (got {marker:?})"
    );
}

/// Secure (selkies) sessions are unaffected: the prefix-intact upstream leg
/// is preserved and the body is not rewritten.
#[tokio::test]
async fn secure_session_still_serves_prefix_intact() {
    let _root = TempDir::new();
    let (upstream, _up) = spawn_legacy_upstream().await;
    let state = build_state(_root.path()).await;
    let (sid, access_token) = seed_session(&state, &upstream, "secure").await;
    let cookie_name = session_cookie_name(&state, &sid);
    let client = TestClient::new(state);

    let mut headers = HashMap::new();
    headers.insert(
        "cookie".to_string(),
        format!("{cookie_name}={access_token}"),
    );
    let resp = client.get_with(&format!("/api/apps/session/{sid}/"), &headers).await;
    assert_eq!(resp.status, StatusCode::OK, "body: {:?}", resp.body);
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(
        body.contains(&format!("<!--UPSTREAM_PATH:/api/apps/session/{sid}/-->")),
        "{body}"
    );
    assert!(body.contains(r#"src="/public/js/rdp.js""#), "{body}");
    assert!(
        !body.contains(&format!("/api/apps/session/{sid}/public/")),
        "{body}"
    );
}

/// First text frame within the window (the WS bridge forwards the fake
/// gclient's marker frame to the client).
async fn first_text(
    stream: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    window: Duration,
) -> Option<String> {
    loop {
        match timeout(window, stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return Some(t.as_str().to_string()),
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => return None,
        }
    }
}
