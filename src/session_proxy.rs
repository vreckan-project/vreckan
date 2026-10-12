//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! The session reverse proxy: forwards `/api/apps/session/{id}/...` requests
//! (HTTP and WebSocket) to each app container.
//!
//! Ports the session-proxy section of `server/api`: `initial_session_auth`
//! (the `?access_token=` first-hit that sets the session cookie and redirects),
//! `session_proxy_subpath` (cookie-authenticated HTTP proxy), and
//! `session_proxy_websocket` (cookie-authenticated WebSocket bridge). The
//! container's own reverse proxy expects the session URL prefix INTACT, so the
//! path is forwarded as-is.

use axum::body::Body;
use axum::extract::{FromRequestParts, Path, State, WebSocketUpgrade};
use axum::http::header;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use http::request::Parts;
use tokio_tungstenite::connect_async;
use tracing::{debug, info, warn};

use crate::state::AppState;

/// The public path prefix this router is mounted under (see `app.rs`). The
/// nested router strips this prefix from `parts.uri.path()`, so handlers that
/// need the full public path (e.g. the first-hit redirect) re-prepend it.
const SESSION_PREFIX: &str = "/api/apps/session";

/// Hop-by-hop / connection-specific headers dropped when proxying. Mirrors
/// `_PROXY_DROP_HEADERS`.
const PROXY_DROP_HEADERS: &[&str] = &[
    "authorization",
    "connection",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "x-upstream-host",
    "x-upstream-auth",
];

/// The per-session cookie name. Mirrors `_session_cookie_name`.
fn session_cookie_name(settings: &crate::settings::Settings, session_id: &str) -> String {
    format!("{}_{}", settings.session_cookie_name, session_id)
}

/// The upstream host (`ip:port`) for a session. Mirrors `_session_upstream_host`.
fn session_upstream_host(session: &serde_json::Value) -> Option<String> {
    let ip = session.get("ip")?.as_str()?;
    let port = session.get("port")?.as_i64()?;
    Some(format!("{ip}:{port}"))
}

/// Constant-time token comparison. Mirrors `secrets.compare_digest`.
fn tokens_equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.as_bytes().iter().zip(b.as_bytes()).all(|(x, y)| x == y)
}

/// Build the upstream request headers: drop hop-by-hop headers, set `Host`,
/// and add the container's Basic auth. Mirrors `_upstream_headers`.
fn upstream_headers(
    request_headers: &HeaderMap,
    session: &serde_json::Value,
    upstream_host: &str,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (key, value) in request_headers.iter() {
        // HeaderName is stored lowercase, so a direct comparison works.
        if PROXY_DROP_HEADERS.contains(&key.as_str()) {
            continue;
        }
        headers.append(key.clone(), value.clone());
    }
    if let Ok(host) = HeaderValue::from_str(upstream_host) {
        headers.insert(header::HOST, host);
    }
    if let (Some(user), Some(pass)) = (
        session.get("custom_user").and_then(|v| v.as_str()),
        session.get("password").and_then(|v| v.as_str()),
    ) {
        let auth = format!("{user}:{pass}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(auth.as_bytes());
        if let Ok(basic) = HeaderValue::from_str(&format!("Basic {b64}")) {
            headers.insert(header::AUTHORIZATION, basic);
        }
    }
    headers
}

/// Reverse-proxy an HTTP request to the session's app container. Mirrors
/// `_proxy_session_request`.
async fn proxy_session_request(
    state: &AppState,
    parts: &Parts,
    body: Body,
    session_id: &str,
) -> Response {
    let session = state.sessions.read().await.get(session_id).cloned();
    let Some(session) = session else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "detail": "Session not found." })),
        )
            .into_response();
    };
    let Some(upstream_host) = session_upstream_host(&session) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "detail": "Session not found." })),
        )
            .into_response();
    };

    // Validate the session cookie.
    let cookie_name = session_cookie_name(&state.settings, session_id);
    let cookie_token = cookie_from_headers(&parts.headers, &cookie_name);
    let expected = session.get("access_token").and_then(|v| v.as_str()).unwrap_or("");
    if !tokens_equal(cookie_token.as_deref().unwrap_or(""), expected) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "detail": "Forbidden: Invalid session or token." })),
        )
            .into_response();
    }

    // The container's reverse proxy expects the session prefix intact. This
    // router is nested under `/api/apps/session` (see `app.rs`), so
    // `parts.uri.path()` is the path *after* that prefix is stripped
    // (e.g. `/{session_id}/...`). Re-prepend the prefix to rebuild the full
    // upstream path, matching Python's `_upstream_path` (which forwards the
    // full `request.url.path` as-is).
    let path = format!("{SESSION_PREFIX}{}", parts.uri.path());
    let mut target_url = format!("http://{upstream_host}{path}");
    if let Some(query) = parts.uri.query() {
        target_url.push_str("?");
        target_url.push_str(query);
    }

    let headers = upstream_headers(&parts.headers, &session, &upstream_host);
    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "detail": format!("Bad request body: {e}") })),
            )
                .into_response();
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({ "detail": format!("Proxy client error: {e}") })),
            )
                .into_response();
        }
    };

    let mut req = client.request(parts.method.clone(), &target_url);
    req = req.headers(headers);
    if !body_bytes.is_empty() {
        req = req.body(body_bytes);
    }

    let upstream_response = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("[{session_id}] Proxy connect to {upstream_host} failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({ "detail": "Could not reach the application container." })),
            )
                .into_response();
        }
    };

    let status = upstream_response.status();
    let mut response_headers = HeaderMap::new();
    for (key, value) in upstream_response.headers().iter() {
        let k = key.as_str();
        if PROXY_DROP_HEADERS.contains(&k) || k == "content-length" {
            continue;
        }
        response_headers.append(key.clone(), value.clone());
    }

    // Stream the raw (still content-encoded) bytes; the client decodes.
    let stream = upstream_response
        .bytes_stream()
        .map(|r| r.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) }));
    let body = Body::from_stream(stream);

    debug!(
        "[{session_id}] Proxied {} {path} -> {upstream_host} ({status})",
        parts.method
    );
    let mut response = Response::builder()
        .status(status)
        .body(body)
        .unwrap_or_else(|_| (
            StatusCode::INTERNAL_SERVER_ERROR,
            "bad proxy response",
        )
            .into_response());
    for (k, v) in response_headers.iter() {
        response.headers_mut().append(k.clone(), v.clone());
    }
    response
        .headers_mut()
        .insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(crate::security::SESSION_CSP),
        );
    response
}

/// Read a cookie value from the `Cookie` header.
fn cookie_from_headers(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?;
    let cookie_str = cookie_header.to_str().ok()?;
    for part in cookie_str.split(';') {
        let kv = part.trim();
        if let Some((k, v)) = kv.split_once('=') {
            if k == name && !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// `GET /api/apps/session/{id}/?access_token=...` — the first hit: validate
/// the token, set the session cookie, and redirect to the cookie-authenticated
/// URL. Mirrors `initial_session_auth`.
async fn initial_session_auth(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    parts: Parts,
    body: Body,
) -> Response {
    let token = parts
        .uri
        .query()
        .and_then(|q| urlencoded_get(q, "access_token"));
    let session = state.sessions.read().await.get(&session_id).cloned();

    // No access_token: the browser presents the session cookie — proxy it.
    let Some(token) = token else {
        return proxy_session_request(&state, &parts, body, &session_id).await;
    };

    let Some(session) = session else {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "detail": "Forbidden: Invalid session or token." })),
        )
            .into_response();
    };
    let expected = session.get("access_token").and_then(|v| v.as_str()).unwrap_or("");
    if !tokens_equal(&token, expected) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "detail": "Forbidden: Invalid session or token." })),
        )
            .into_response();
    }

    // Honor a TLS-terminating reverse proxy for the redirect scheme.
    let scheme = parts
        .headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    // The redirect must be an *absolute* URL (scheme + host + path). Python
    // builds it from `request.url` (which carries the host and the full path);
    // here we take the public host from the request. Under HTTP/2 the host
    // arrives in the `:authority` pseudo-header (stored in the URI), not a
    // `Host` header, so we resolve it via the shared `public_host` helper.
    let host = crate::auth::public_host(&parts.headers, &parts.uri);
    // This router is nested under `/api/apps/session` (see `app.rs`), so
    // `parts.uri.path()` is the path *after* that prefix is stripped
    // (e.g. `/{session_id}/`). Re-prepend the prefix to rebuild the full
    // public path, matching Python's `request.url`.
    let path = format!("{SESSION_PREFIX}{}", parts.uri.path());
    let mut redirect = format!("{scheme}://{host}{path}");
    if let Some(query) = parts.uri.query() {
        let filtered = filter_query_param(query, "access_token");
        if !filtered.is_empty() {
            redirect.push_str("?");
            redirect.push_str(&filtered);
        }
    }

    let is_embedded = urlencoded_get(parts.uri.query().unwrap_or(""), "embedded")
        .as_deref()
        == Some("true");
    let samesite = if is_embedded { "none" } else { "lax" };
    let cookie_name = session_cookie_name(&state.settings, &session_id);
    let set_cookie = format!(
        "{cookie_name}={token}; Path=/api/apps/session/{session_id}; HttpOnly; Secure; SameSite={samesite}"
    );

    let mut response = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, redirect)
        .header(header::SET_COOKIE, set_cookie)
        .body(axum::body::Body::from(
            serde_json::to_vec(&serde_json::json!({ "detail": "Redirecting." })).unwrap_or_default(),
        ))
        .unwrap_or_default();

    // Collaboration token cookie (if present and this is a collab session).
    if let Some(collab_token) = urlencoded_get(parts.uri.query().unwrap_or(""), "token") {
        if session.get("is_collaboration").and_then(|v| v.as_bool()).unwrap_or(false) {
            let collab_cookie = format!(
                "collab_token_{session_id}={collab_token}; Path=/api/apps/session/{session_id}; HttpOnly; Secure; SameSite=none"
            );
            if let Ok(hv) = HeaderValue::from_str(&collab_cookie) {
                response.headers_mut().append(header::SET_COOKIE, hv);
            }
        }
    }

    info!("[{session_id}] Initial auth successful. Setting session cookie and redirecting.");
    response
}

/// `GET /api/apps/session/{id}/{path}` — proxy cookie-authenticated sub-path
/// requests. Mirrors `session_proxy_subpath`.
/// Sub-path handler: dispatches to the WebSocket bridge when the request is a
/// WebSocket upgrade, otherwise to the plain-HTTP proxy. In Python these are
/// two separate route types (HTTP vs WebSocket); in axum both are `any()` on
/// the same path, so a single dispatcher is required.
async fn session_proxy_subpath(
    State(state): State<AppState>,
    // The route is `/{session_id}/{*path}` — two capture groups. We only use
    // `session_id` here (the forwarded path comes from `parts.uri.path()`),
    // but both must be extracted or axum rejects the request with "Wrong
    // number of path arguments".
    Path((session_id, _path)): Path<(String, String)>,
    parts: Parts,
    body: Body,
) -> Response {
    let is_ws = parts
        .headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if is_ws {
        session_proxy_websocket(&state, parts, session_id).await
    } else {
        proxy_session_request(&state, &parts, body, &session_id).await
    }
}

/// WebSocket bridge: client <-> upstream. Mirrors `_proxy_session_websocket`.
///
/// Builds the `WebSocketUpgrade` from the request parts (the axum extractor
/// would reject a plain-HTTP request, so we build it manually and fall back to
/// the HTTP proxy when the upgrade headers are absent).
async fn session_proxy_websocket(
    state: &AppState,
    mut parts: Parts,
    session_id: String,
) -> Response {
    let mut upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, state).await {
        Ok(u) => u,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let session = state.sessions.read().await.get(&session_id).cloned();
    let Some(session) = session else {
        return upgrade.on_upgrade(|mut ws| async move {
            let _ = ws.close().await;
        });
    };
    let Some(upstream_host) = session_upstream_host(&session) else {
        return upgrade.on_upgrade(|mut ws| async move {
            let _ = ws.close().await;
        });
    };

    // Validate the session cookie.
    let cookie_name = session_cookie_name(&state.settings, &session_id);
    let cookie_token = cookie_from_headers(&parts.headers, &cookie_name);
    let expected = session.get("access_token").and_then(|v| v.as_str()).unwrap_or("");
    if !tokens_equal(cookie_token.as_deref().unwrap_or(""), expected) {
        return upgrade.on_upgrade(|mut ws| async move {
            let _ = ws.close().await;
        });
    }

    // The client's offered subprotocols.
    let client_subprotocols: Vec<String> = upgrade
        .requested_protocols()
        .filter_map(|p| p.to_str().ok().map(|s| s.to_string()))
        .collect();

    // Build the upstream WebSocket request. tungstenite's client handshake
    // (`generate_request`) *requires* the five handshake headers to already be
    // present in the request — it does not synthesize them (unlike Python's
    // `websockets.connect`). So we set them explicitly: a fresh random
    // `Sec-WebSocket-Key`, the upstream `Host`, and the fixed
    // `Connection`/`Upgrade`/`Sec-WebSocket-Version` values. The client's own
    // handshake headers are dropped (its key is bound to *its* connection).
    // Re-prepend the session prefix (stripped by the nested router) so the
    // container's nginx sees the full path, matching Python's `_upstream_path`.
    let path = format!("{SESSION_PREFIX}{}", parts.uri.path());
    let mut target_url = format!("ws://{upstream_host}{path}");
    if let Some(query) = parts.uri.query() {
        target_url.push_str("?");
        target_url.push_str(query);
    }
    let mut ws_headers = HeaderMap::new();
    for (key, value) in parts.headers.iter() {
        let k = key.as_str().to_ascii_lowercase();
        // Drop the client's handshake headers (we set our own below) and the
        // upstream-specific auth headers.
        if k.starts_with("sec-websocket-")
            || matches!(
                k.as_str(),
                "authorization" | "host" | "connection" | "upgrade" | "x-upstream-host" | "x-upstream-auth"
            )
        {
            continue;
        }
        ws_headers.append(key.clone(), value.clone());
    }
    // The five headers tungstenite requires. A fresh random 16-byte key,
    // base64-encoded, per RFC 6455.
    let ws_key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
    if let Ok(hv) = HeaderValue::from_str(&ws_key) {
        ws_headers.insert(header::SEC_WEBSOCKET_KEY, hv);
    }
    if let Ok(hv) = HeaderValue::from_str(&upstream_host) {
        ws_headers.insert(header::HOST, hv);
    }
    ws_headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    ws_headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    ws_headers.insert(header::SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
    if let (Some(user), Some(pass)) = (
        session.get("custom_user").and_then(|v| v.as_str()),
        session.get("password").and_then(|v| v.as_str()),
    ) {
        let auth = format!("{user}:{pass}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(auth.as_bytes());
        if let Ok(basic) = HeaderValue::from_str(&format!("Basic {b64}")) {
            ws_headers.insert(header::AUTHORIZATION, basic);
        }
    }

    let mut request = http::Request::builder()
        .method(Method::GET)
        .uri(&target_url)
        .body(())
        .unwrap_or_else(|_| http::Request::new(()));
    *request.headers_mut() = ws_headers;
    for sp in &client_subprotocols {
        if let Ok(hv) = HeaderValue::from_str(sp) {
            request.headers_mut().append(header::SEC_WEBSOCKET_PROTOCOL, hv);
        }
    }

    let (upstream_ws, upstream_response) = match connect_async(request).await {
        Ok(r) => r,
        Err(e) => {
            warn!("[{session_id}] WebSocket connect to {upstream_host} failed: {e}");
            return upgrade.on_upgrade(|mut ws| async move {
                let _ = ws.close().await;
            });
        }
    };

    // Echo a subprotocol only if the client actually offered it (RFC 6455).
    if let Some(negotiated) = upstream_response
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
    {
        if client_subprotocols.iter().any(|s| s == negotiated) {
            if let Ok(hv) = HeaderValue::from_str(negotiated) {
                upgrade.set_selected_protocol(hv);
            }
        }
    }

    upgrade.on_upgrade(move |client_ws| async move {
        // split() returns (SplitSink [write half], SplitStream [read half]).
        let (mut client_tx, mut client_rx) = client_ws.split();
        let (mut upstream_tx, mut upstream_rx) = upstream_ws.split();

        // Relay messages in both directions. `biased;` polls the upstream ->
        // client branch first so video/audio frames are prioritized over input
        // events, keeping the desktop responsive under heavy input load. When
        // either direction ends, the loop breaks and all four halves are
        // dropped, closing both connections (mirrors Python's
        // `asyncio.wait(..., FIRST_COMPLETED)` + cancel).
        loop {
            tokio::select! {
                biased;
                // Upstream -> client.
                upstream_msg = upstream_rx.next() => {
                    match upstream_msg {
                        Some(Ok(msg)) => {
                            if let Some(amsg) = tungstenite_to_axum(msg) {
                                if client_tx.send(amsg).await.is_err() {
                                    break;
                                }
                            }
                        }
                        _ => {
                            // Upstream closed: send a clean close frame so the
                            // browser sees a normal (1000) closure rather than
                            // an abnormal (1006) one.
                            let _ = client_tx
                                .send(axum::extract::ws::Message::Close(None))
                                .await;
                            break;
                        }
                    }
                }
                // Client -> upstream.
                client_msg = client_rx.next() => {
                    match client_msg {
                        Some(Ok(msg)) => {
                            let tmsg = axum_to_tungstenite(msg);
                            if upstream_tx.send(tmsg).await.is_err() {
                                break;
                            }
                        }
                        _ => break, // client disconnected
                    }
                }
            }
        }
    })
}

/// Convert an axum WebSocket message to a tungstenite one.
fn axum_to_tungstenite(msg: axum::extract::ws::Message) -> tokio_tungstenite::tungstenite::Message {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    match msg {
        axum::extract::ws::Message::Text(t) => {
            tokio_tungstenite::tungstenite::Message::Text(t.as_str().to_string().into())
        }
        axum::extract::ws::Message::Binary(b) => {
            tokio_tungstenite::tungstenite::Message::Binary(b)
        }
        axum::extract::ws::Message::Ping(b) => tokio_tungstenite::tungstenite::Message::Ping(b),
        axum::extract::ws::Message::Pong(b) => tokio_tungstenite::tungstenite::Message::Pong(b),
        axum::extract::ws::Message::Close(Some(cf)) => {
            tokio_tungstenite::tungstenite::Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::frame::CloseFrame {
                    code: CloseCode::from(cf.code),
                    reason: cf.reason.as_str().to_string().into(),
                },
            ))
        }
        axum::extract::ws::Message::Close(None) => {
            tokio_tungstenite::tungstenite::Message::Close(None)
        }
    }
}

/// Convert a tungstenite WebSocket message to an axum one (None for raw frames).
fn tungstenite_to_axum(
    msg: tokio_tungstenite::tungstenite::Message,
) -> Option<axum::extract::ws::Message> {
    Some(match msg {
        tokio_tungstenite::tungstenite::Message::Text(t) => {
            axum::extract::ws::Message::Text(t.as_str().to_string().into())
        }
        tokio_tungstenite::tungstenite::Message::Binary(b) => {
            axum::extract::ws::Message::Binary(b)
        }
        tokio_tungstenite::tungstenite::Message::Ping(b) => axum::extract::ws::Message::Ping(b),
        tokio_tungstenite::tungstenite::Message::Pong(b) => axum::extract::ws::Message::Pong(b),
        tokio_tungstenite::tungstenite::Message::Close(Some(cf)) => {
            axum::extract::ws::Message::Close(Some(axum::extract::ws::CloseFrame {
                code: u16::from(cf.code),
                reason: cf.reason.as_str().to_string().into(),
            }))
        }
        tokio_tungstenite::tungstenite::Message::Close(None) => {
            axum::extract::ws::Message::Close(None)
        }
        // Raw frames are not real messages; skip them.
        tokio_tungstenite::tungstenite::Message::Frame(_) => return None,
    })
}

/// Parse a single value from a URL-encoded query string.
fn urlencoded_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

/// Remove a single param from a URL-encoded query string.
fn filter_query_param(query: &str, key: &str) -> String {
    query
        .split('&')
        .filter(|pair| pair.split_once('=').map(|(k, _)| k != key).unwrap_or(true))
        .collect::<Vec<_>>()
        .join("&")
}

/// Minimal percent-decoding for query values.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The session-proxy router. Mirrors the three session-proxy routes in
/// `server/api`. Mounted under `/api/apps/session` (see `app.rs`).
pub fn session_proxy_router() -> Router<AppState> {
    Router::new()
        .route("/{session_id}/", any(initial_session_auth))
        .route("/{session_id}/{*path}", any(session_proxy_subpath))
}
