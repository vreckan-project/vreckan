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

use std::collections::HashMap;

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

use crate::session_shares::SessionShareCaps;
use crate::state::AppState;
use crate::tokens::tokens_equal;

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

/// The file API surface of the selkies web client, in-container (after the
/// session prefix), enumerated from a running linuxserver selkies container
/// on 2026-10-17 (ANT-306, container `ant-306-endpoints`):
///
/// - `api/files/...` — the file-manager tree (nginx `/api` → the selkies
///   backend). A directory answers with a styled HTML listing, a file with
///   its bytes and `Content-Disposition: attachment`, and a directory
///   requested without its trailing slash is 301-redirected to the
///   slash-terminated listing. One route serves both the *browse* (listing)
///   and *download* (file) capabilities, so the required capability is
///   decided from the upstream response, not the URL
///   (`files_response_cap`).
/// - `api/upload` — the client upload endpoint: a POST whose body is the
///   file and whose destination rides the `X-Upload-Path` header (chunked
///   transfers add `X-Upload-Id`/`-Offset`/`-Total`/`-Final`).
///
/// Adjacent paths the web client also touches — `api/status`, `api/switch`,
/// `api/turn`, `api/print/{name}`, `api/recording`, `api/screenshot`, the
/// data-plane `api/websockets`/`api/webrtc/signaling`, and `pelorus/*` — are
/// stream and control-plane plumbing rather than file-manager access, so v1
/// deliberately does not gate them.
///
/// Named constants keep an image that moves these routes to a constant
/// change here.
const SELKIES_FILE_API: &str = "api/files/";
/// The no-slash root of the file tree: the container 301s it to
/// `SELKIES_FILE_API`, so it is the browse side too (ANT-322, F6).
const SELKIES_FILE_API_ROOT: &str = "api/files";
const SELKIES_FILE_UPLOAD: &str = "api/upload";

/// A file-manager capability the session proxy enforces for share visitors
/// (the selkies web client's file API, WP3 of the share-session links
/// feature).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileCap {
    /// Listing the file tree (`api/files/` directory responses).
    Browse,
    /// Fetching a file's bytes (`api/files/<file>`).
    Download,
    /// Storing a file (`api/upload`).
    Upload,
}

/// Whether the share's capability set covers the required file capability.
fn caps_allow(caps: &SessionShareCaps, cap: FileCap) -> bool {
    match cap {
        FileCap::Browse => caps.file_browse,
        FileCap::Download => caps.file_download,
        FileCap::Upload => caps.file_upload,
    }
}

/// The capability the files route requires for this upstream response: a
/// served file (its `Content-Disposition: attachment`) needs *download*;
/// everything else the route answers with — an HTML listing, the no-slash
/// 301 (the root or a directory subpath), a 404, the container's own 403 —
/// is the *browse* side.
fn files_response_cap(in_container_path: &str, headers: &HeaderMap) -> Option<FileCap> {
    if !in_container_path.starts_with(SELKIES_FILE_API) && in_container_path != SELKIES_FILE_API_ROOT {
        return None;
    }
    let download = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("attachment"));
    Some(if download { FileCap::Download } else { FileCap::Browse })
}

/// The in-container path for a proxied request: everything after
/// `/{session_id}/` in the (nested-router-stripped) public path — what the
/// container's own routes see.
fn in_container_path(uri: &axum::http::Uri, session_id: &str) -> String {
    let path = uri.path();
    let prefix = format!("/{session_id}/");
    path.strip_prefix(&prefix)
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string())
}

/// The upstream host (`ip:port`) for a session. Mirrors `_session_upstream_host`.
fn session_upstream_host(session: &serde_json::Value) -> Option<String> {
    let ip = session.get("ip")?.as_str()?;
    let port = session.get("port")?.as_i64()?;
    Some(format!("{ip}:{port}"))
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

    // Validate the session cookie (the owner's bearer token) or, for a
    // collaboration session, the collab token presented via the
    // `collab_token_{id}` cookie or a first-hit `?token=` (the controller or a
    // viewer, as set by the room page). A per-visit session-share token
    // (ANT-305) is admitted too, and its share's capability set is resolved
    // for the file-API gate. See `resolve_visitor_access`.
    let cookie_name = session_cookie_name(&state.settings, session_id);
    let cookie_token = cookie_from_headers(&parts.headers, &cookie_name);
    let collab_token = collab_token_from_request(&parts, session_id);
    let cookie_token_ref = cookie_token.as_deref().unwrap_or("");
    let (grants, caps) =
        resolve_visitor_access(state, &session, session_id, cookie_token_ref, &collab_token).await;
    if !grants {
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
    let in_container_path = in_container_path(&parts.uri, session_id);

    // WP3 gate, share visitors only: refuse the upload endpoint up front so
    // a visitor without the cap is not charged the upload body. Owner and
    // room-collab traffic (`caps == None`) passes everything.
    if let Some(caps) = &caps {
        if in_container_path == SELKIES_FILE_UPLOAD && !caps_allow(caps, FileCap::Upload) {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({ "detail": file_cap_detail(FileCap::Upload) })),
            )
                .into_response();
        }
    }
    let mut target_url = format!("http://{upstream_host}{path}");
    let query = parts.uri.query().unwrap_or("");
    if !query.is_empty() {
        target_url.push('?');
        target_url.push_str(query);
    }
    // In selkies "secure mode" the container validates every connection
    // against its token table, so present the appropriate token: the
    // visitor's collab token, or the owner's container token. A token
    // already in the query (e.g. the room iframe's URL) is left as-is.
    if let Some(upstream_token) =
        upstream_token_for(&session, cookie_token.as_deref().unwrap_or(""), &collab_token)
    {
        if urlencoded_get(query, "token").is_none() {
            target_url.push_str(if query.is_empty() { "?" } else { "&" });
            target_url.push_str(&format!("token={upstream_token}"));
        }
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

    // WP3 gate (continued), share visitors only: the files route serves
    // listings and file downloads from one path, so the required capability
    // is classified from the upstream response — a served file carries
    // `Content-Disposition: attachment`, a listing or the no-slash directory
    // 301 does not. A violation drops the in-flight upstream response and
    // answers the visitor with the 403.
    if let (Some(caps), Some(cap)) =
        (caps.as_ref(), files_response_cap(&in_container_path, upstream_response.headers()))
    {
        if !caps_allow(caps, cap) {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({ "detail": file_cap_detail(cap) })),
            )
                .into_response();
        }
    }

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
    // Multiple `Cookie` headers are legal (proxy chains, or a client that
    // sends the session cookie and a collab cookie separately); scan them all.
    for cookie_header in headers.get_all(header::COOKIE) {
        let cookie_str = match cookie_header.to_str().ok() {
            Some(s) => s,
            None => continue,
        };
        for part in cookie_str.split(';') {
            let kv = part.trim();
            if let Some((k, v)) = kv.split_once('=') {
                if k == name && !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// The collaboration token presented for a session proxy request: the
/// `collab_token_{id}` cookie (set by the room page and the first-hit
/// redirect) if present, otherwise the first-hit `?token=` query param. Empty
/// when neither is present.
fn collab_token_from_request(parts: &Parts, session_id: &str) -> String {
    let cookie_name = format!("collab_token_{session_id}");
    if let Some(v) = cookie_from_headers(&parts.headers, &cookie_name) {
        return v;
    }
    parts
        .uri
        .query()
        .and_then(|q| urlencoded_get(q, "token"))
        .unwrap_or_default()
}

/// True when `token` is a valid collaboration token for the session: it equals
/// the controller token or matches one of the viewers' tokens. Plain sessions
/// carry no collaboration tokens unless a share has promoted them to selkies
/// secure mode (a non-empty `master_token`, ANT-305), in which case the same
/// rules apply.
fn collab_token_is_valid(session: &serde_json::Value, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let is_collab = session
        .get("is_collaboration")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let promoted = session
        .get("master_token")
        .and_then(|v| v.as_str())
        .map(|m| !m.is_empty())
        .unwrap_or(false);
    if !is_collab && !promoted {
        return false;
    }
    let controller = session
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !controller.is_empty() && tokens_equal(token, controller) {
        return true;
    }
    session
        .get("viewers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|v| {
                tokens_equal(token, v.get("token").and_then(|t| t.as_str()).unwrap_or(""))
            })
        })
        .unwrap_or(false)
}

/// True when the session runs the selkies container in "secure mode" (a
/// master token is present): the container then validates every data-plane
/// connection against its token table, so the proxy must present a token.
fn session_is_secure_mode(session: &serde_json::Value) -> bool {
    session
        .get("master_token")
        .and_then(|v| v.as_str())
        .map(|m| !m.is_empty())
        .unwrap_or(false)
}

/// The token to present to the container for this request, when the session
/// is in secure mode: the presented collab token if it is valid, otherwise
/// the owner's container token (the controller token for rooms, the access
/// token for promoted plain sessions). `None` for legacy sessions — the
/// container does not validate tokens there.
fn upstream_token_for(
    session: &serde_json::Value,
    cookie_token: &str,
    collab_token: &str,
) -> Option<String> {
    if !session_is_secure_mode(session) {
        return None;
    }
    if !collab_token.is_empty() && collab_token_is_valid(session, collab_token) {
        return Some(collab_token.to_string());
    }
    let access = session
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !cookie_token.is_empty() && tokens_equal(cookie_token, access) {
        if session.get("is_collaboration").and_then(|v| v.as_bool()).unwrap_or(false) {
            let controller = session
                .get("controller_token")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !controller.is_empty() {
                return Some(controller.to_string());
            }
        }
        return Some(access.to_string());
    }
    None
}

/// The capability set of a per-visit token minted for a session share of
/// this session (ANT-305), `None` when the token is not one. The vreckan-side
/// token map is the source of truth for tokens whose session record was not
/// promoted to secure mode (the container cannot know about them); WP3
/// resolves a visitor's capabilities through the same map.
///
/// The caps are a *snapshot* taken when the visit was minted (see
/// `SessionShareTokenRecord`): a mid-visit edit to the share's caps applies
/// on the visitor's next open, not live. Revocation is the exception — it
/// removes the record, so a revoked link cuts its visits immediately.
async fn visitor_share_caps(
    state: &AppState,
    session_id: &str,
    token: &str,
) -> Option<SessionShareCaps> {
    if token.is_empty() {
        return None;
    }
    state
        .session_share_tokens
        .read()
        .await
        .get(token)
        .filter(|r| r.session_id == session_id)
        .map(|r| r.caps.clone())
}

/// The pure core of the session-proxy credential check, testable without a
/// database. Returns whether the request is admitted and, when the
/// credential is a per-visit share token, the share's capability set that
/// gates the in-container file API (WP3); the owner's session cookie and
/// room collaboration tokens are never gated.
///
/// Precedence mirrors the historical check: the owner's session cookie
/// first; then a valid collaboration token — a *promoted* share visitor's
/// per-visit token is both a valid collab token (it sits in the session's
/// viewers list) and a share token, and the `share_caps` lookup is what
/// carries the gate for it; then the share-token fallbacks for legacy
/// (non-secure-mode) sessions, where the token is not in the session's
/// viewers list and only the vreckan-side map knows it.
fn resolve_access(
    session: &serde_json::Value,
    cookie_token: &str,
    collab_token: &str,
    share_caps: &HashMap<String, SessionShareCaps>,
) -> (bool, Option<SessionShareCaps>) {
    let expected = session.get("access_token").and_then(|v| v.as_str()).unwrap_or("");
    if !cookie_token.is_empty() && tokens_equal(cookie_token, expected) {
        return (true, None);
    }
    if !collab_token.is_empty() && collab_token_is_valid(session, collab_token) {
        return (true, share_caps.get(collab_token).cloned());
    }
    if !collab_token.is_empty() {
        if let Some(caps) = share_caps.get(collab_token) {
            return (true, Some(caps.clone()));
        }
    }
    if !cookie_token.is_empty() {
        if let Some(caps) = share_caps.get(cookie_token) {
            return (true, Some(caps.clone()));
        }
    }
    (false, None)
}

/// Resolve a session-proxy request's credential and the share capability set
/// that gates its in-container file API. See `resolve_access`.
async fn resolve_visitor_access(
    state: &AppState,
    session: &serde_json::Value,
    session_id: &str,
    cookie_token: &str,
    collab_token: &str,
) -> (bool, Option<SessionShareCaps>) {
    let mut share_caps = HashMap::new();
    for token in [collab_token, cookie_token] {
        if let Some(caps) = visitor_share_caps(state, session_id, token).await {
            share_caps.insert(token.to_string(), caps);
        }
    }
    resolve_access(session, cookie_token, collab_token, &share_caps)
}

/// The 403 body for a file-capability violation (WP3).
fn file_cap_detail(cap: FileCap) -> &'static str {
    match cap {
        FileCap::Browse => "Forbidden: File browsing is disabled for this share.",
        FileCap::Download => "Forbidden: File download is disabled for this share.",
        FileCap::Upload => "Forbidden: File upload is disabled for this share.",
    }
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
    // A collab/share-token first hit (e.g. a session-share redirect) is
    // proxied too; when the token is valid for the session, the collab token
    // cookie is set so the proxied page's sub-requests (file API, WebSocket
    // re-connections) authenticate by cookie rather than repeating the
    // token in every URL (ANT-305).
    let Some(token) = token else {
        let collab_token =
            urlencoded_get(parts.uri.query().unwrap_or(""), "token").unwrap_or_default();
        let mut response = proxy_session_request(&state, &parts, body, &session_id).await;
        let grant = session
            .as_ref()
            .is_some_and(|s| collab_token_is_valid(s, &collab_token))
            || state
                .session_share_tokens
                .read()
                .await
                .contains_key(&collab_token);
        if !collab_token.is_empty() && grant {
            let set_cookie = format!(
                "collab_token_{session_id}={collab_token}; Path=/api/apps/session/{session_id}; HttpOnly; Secure; SameSite=none"
            );
            if let Ok(hv) = HeaderValue::from_str(&set_cookie) {
                response.headers_mut().append(header::SET_COOKIE, hv);
            }
        }
        return response;
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
            redirect.push('?');
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

    // Collaboration token cookie (collab sessions, and session-share tokens
    // on promoted plain sessions — ANT-305).
    if let Some(collab_token) = urlencoded_get(parts.uri.query().unwrap_or(""), "token") {
        let valid = session.get("is_collaboration").and_then(|v| v.as_bool()).unwrap_or(false)
            || collab_token_is_valid(&session, &collab_token)
            || state
                .session_share_tokens
                .read()
                .await
                .contains_key(&collab_token);
        if valid {
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

    // Validate the session cookie (the owner's bearer token) or, for a
    // collaboration session, the collab token presented via the
    // `collab_token_{id}` cookie or a first-hit `?token=` (the controller or a
    // viewer). A per-visit session-share token (ANT-305) is admitted too; the
    // data-plane sockets it opens are not file-API paths, so the share's
    // capability set is not consulted here. See `resolve_visitor_access`.
    let cookie_name = session_cookie_name(&state.settings, &session_id);
    let cookie_token = cookie_from_headers(&parts.headers, &cookie_name);
    let collab_token = collab_token_from_request(&parts, &session_id);
    let cookie_token_ref = cookie_token.as_deref().unwrap_or("");
    let (grants, _caps) =
        resolve_visitor_access(state, &session, &session_id, cookie_token_ref, &collab_token).await;
    if !grants {
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
    let query = parts.uri.query().unwrap_or("");
    if !query.is_empty() {
        target_url.push('?');
        target_url.push_str(query);
    }
    // Secure-mode containers validate the data WebSocket against the token
    // table too (`?token=`); present the appropriate one (same rules as the
    // HTTP path).
    let upstream_token =
        upstream_token_for(&session, cookie_token.as_deref().unwrap_or(""), &collab_token);
    if let Some(upstream_token) = &upstream_token {
        if urlencoded_get(query, "token").is_none() {
            target_url.push_str(if query.is_empty() { "?" } else { "&" });
            target_url.push_str(&format!("token={upstream_token}"));
        }
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

    // Data-plane connect tracking (ANT-443): only plain secure sessions
    // need it — rooms run their own mk-transfer flow, and legacy sessions
    // have no token table to re-derive. The upstream leg already succeeded
    // here, so selkies accepted this token.
    let track_conns = !session
        .get("is_collaboration")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && session
            .get("master_token")
            .and_then(|v| v.as_str())
            .map(|m| !m.is_empty())
            .unwrap_or(false);

    let state = state.clone();
    upgrade.on_upgrade(move |client_ws| async move {
        // Record this client's connect (reconnects keep one entry per token,
        // re-stamped with a newer sequence) and re-derive the mk grant: the
        // connection may just have changed who the most recent controller is.
        let track_token = if track_conns { upstream_token.clone() } else { None };
        if let Some(token) = &track_token {
            let seq = state
                .selkies_conn_seq
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut conns = state.selkies_client_conns.write().await;
            let list = conns.entry(session_id.clone()).or_default();
            list.retain(|(t, _)| t != token);
            list.push((token.clone(), seq));
        }
        if track_conns {
            crate::collaboration::refresh_plain_session_token_table(&state, &session_id).await;
        }
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
        // Bridge over (either side closed — including the selkies server
        // killing a superseded primary): drop this client's record and
        // re-derive the mk grant. A disconnect can leave the remaining
        // connected controller as the most recent one (ANT-443).
        if track_conns {
            if let Some(token) = &track_token {
                let mut conns = state.selkies_client_conns.write().await;
                if let Some(list) = conns.get_mut(&session_id) {
                    if let Some(pos) = list.iter().rposition(|(t, _)| t == token) {
                        list.remove(pos);
                    }
                    if list.is_empty() {
                        conns.remove(&session_id);
                    }
                }
            }
            crate::collaboration::refresh_plain_session_token_table(&state, &session_id).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(input: bool, browse: bool, download: bool, upload: bool) -> SessionShareCaps {
        SessionShareCaps {
            input,
            file_browse: browse,
            file_download: download,
            file_upload: upload,
        }
    }

    fn session_value(
        is_collab: bool,
        master_token: &str,
        access_token: &str,
        controller_token: &str,
        viewer_tokens: &[&str],
    ) -> serde_json::Value {
        let arr = viewer_tokens
            .iter()
            .map(|t| serde_json::json!({ "token": t, "permission": "readonly" }))
            .collect::<Vec<_>>();
        serde_json::json!({
            "access_token": access_token,
            "is_collaboration": is_collab,
            "master_token": master_token,
            "controller_token": controller_token,
            "viewers": serde_json::Value::Array(arr),
        })
    }

    #[test]
    fn resolve_access_owner_cookie_ungated() {
        let session = session_value(false, "", "owner-tok", "", &[]);
        let (grants, c) = resolve_access(&session, "owner-tok", "", &HashMap::new());
        assert!(grants);
        assert!(c.is_none(), "owner traffic is never capability-gated");
    }

    #[test]
    fn resolve_access_room_collab_ungated() {
        let session = session_value(true, "master", "owner-tok", "ctrl-tok", &["view1"]);
        let (g, c) = resolve_access(&session, "", "ctrl-tok", &HashMap::new());
        assert!(g);
        assert!(c.is_none(), "room collab tokens are ungated (MK/slot model)");
        let (g, c) = resolve_access(&session, "", "view1", &HashMap::new());
        assert!(g);
        assert!(c.is_none());
    }

    #[test]
    fn resolve_access_promoted_share_visitor_gated() {
        // A promoted-session share visitor authenticates through the collab
        // path (the visit token is in the session's viewers list); the
        // share-token map is what carries the gate.
        let session = session_value(false, "master", "owner-tok", "", &["visit1"]);
        let mut share_caps = HashMap::new();
        share_caps.insert("visit1".to_string(), caps(false, true, false, false));
        let (g, c) = resolve_access(&session, "", "visit1", &share_caps);
        assert!(g);
        assert_eq!(c, Some(caps(false, true, false, false)));
    }

    #[test]
    fn resolve_access_legacy_share_token_via_collab_channel() {
        // Non-promoted session: the visit token is not in the session's
        // viewers list, so only the vreckan-side map admits it.
        let session = session_value(false, "", "owner-tok", "", &[]);
        let mut share_caps = HashMap::new();
        share_caps.insert("legacy1".to_string(), caps(false, false, true, false));
        let (g, c) = resolve_access(&session, "", "legacy1", &share_caps);
        assert!(g);
        assert_eq!(c, Some(caps(false, false, true, false)));
    }

    #[test]
    fn resolve_access_share_token_in_session_cookie() {
        let session = session_value(false, "", "owner-tok", "", &[]);
        let mut share_caps = HashMap::new();
        share_caps.insert("cookie-share".to_string(), caps(false, true, true, false));
        let (g, c) = resolve_access(&session, "cookie-share", "", &share_caps);
        assert!(g);
        assert_eq!(c, Some(caps(false, true, true, false)));
    }

    #[test]
    fn resolve_access_owner_wins_over_share_token() {
        let session = session_value(false, "", "owner-tok", "", &[]);
        let mut share_caps = HashMap::new();
        share_caps.insert("owner-tok".to_string(), caps(false, false, false, false));
        let (g, c) = resolve_access(&session, "owner-tok", "", &share_caps);
        assert!(g);
        assert!(c.is_none(), "the owner's cookie must not be capability-gated");
    }

    #[test]
    fn resolve_access_unknown_tokens_forbidden() {
        let session = session_value(true, "master", "owner-tok", "ctrl", &["view1"]);
        let (g, _) = resolve_access(&session, "nope", "other", &HashMap::new());
        assert!(!g);
        let (g, _) = resolve_access(&session, "", "", &HashMap::new());
        assert!(!g);
    }

    #[test]
    fn resolve_access_collab_branch_precedence() {
        // A valid collab token that is also a share token is resolved through
        // the collab branch, ahead of a (different) share token in the
        // session cookie.
        let session = session_value(false, "master", "owner-tok", "", &["visit1"]);
        let mut share_caps = HashMap::new();
        share_caps.insert("visit1".to_string(), caps(false, true, true, false));
        share_caps.insert("cookie-share".to_string(), caps(false, false, false, false));
        let (g, c) = resolve_access(&session, "cookie-share", "visit1", &share_caps);
        assert!(g);
        assert_eq!(c, Some(caps(false, true, true, false)));
    }

    #[test]
    fn files_response_cap_classification() {
        let mut listing = HeaderMap::new();
        listing.insert(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );
        assert_eq!(
            files_response_cap("api/files/", &listing),
            Some(FileCap::Browse)
        );
        assert_eq!(
            files_response_cap("api/files/sub/", &listing),
            Some(FileCap::Browse)
        );
        // A no-slash directory request 301s to its slash-terminated listing;
        // the redirect carries no disposition, so it is the browse side.
        let mut redirect = HeaderMap::new();
        redirect.insert(
            header::LOCATION,
            "/api/apps/session/s/api/files/sub/".parse().unwrap(),
        );
        assert_eq!(
            files_response_cap("api/files/sub", &redirect),
            Some(FileCap::Browse)
        );

        let mut file = HeaderMap::new();
        file.insert(
            header::CONTENT_DISPOSITION,
            r#"attachment; filename="hello.txt""#.parse().unwrap(),
        );
        assert_eq!(
            files_response_cap("api/files/hello.txt", &file),
            Some(FileCap::Download)
        );
        let mut file_caps = HeaderMap::new();
        file_caps.insert(
            header::CONTENT_DISPOSITION,
            r#"Attachment; filename="x""#.parse().unwrap(),
        );
        assert_eq!(
            files_response_cap("api/files/x", &file_caps),
            Some(FileCap::Download),
            "disposition matching is case-insensitive"
        );
        let mut inline = HeaderMap::new();
        inline.insert(header::CONTENT_DISPOSITION, "inline".parse().unwrap());
        assert_eq!(
            files_response_cap("api/files/x", &inline),
            Some(FileCap::Browse)
        );

        // Non-file routes are not gated.
        assert_eq!(files_response_cap("api/upload", &listing), None);
        assert_eq!(files_response_cap("index.html", &listing), None);
        assert_eq!(files_response_cap("api/status", &listing), None);
        // The no-slash root 301s to the slash-terminated listing; the
        // redirect is the browse side, so it is gated like any listing
        // (ANT-322, F6).
        let mut root_redirect = HeaderMap::new();
        root_redirect.insert(
            header::LOCATION,
            "/api/apps/session/s/api/files/".parse().unwrap(),
        );
        assert_eq!(
            files_response_cap("api/files", &root_redirect),
            Some(FileCap::Browse),
            "the no-slash root is the browse side"
        );
        assert_eq!(
            files_response_cap("api/files", &listing),
            Some(FileCap::Browse),
            "the no-slash root is the browse side regardless of headers"
        );
    }

    #[test]
    fn in_container_path_strips_session_segment() {
        let uri: axum::http::Uri = "/sid123/api/files/config/Desktop/".parse().unwrap();
        assert_eq!(in_container_path(&uri, "sid123"), "api/files/config/Desktop/");
        let uri: axum::http::Uri = "/sid123/api/upload".parse().unwrap();
        assert_eq!(in_container_path(&uri, "sid123"), "api/upload");
        let uri: axum::http::Uri = "/sid123/".parse().unwrap();
        assert_eq!(in_container_path(&uri, "sid123"), "");
    }

    #[test]
    fn caps_allow_matrix() {
        let all = caps(true, true, true, true);
        assert!(caps_allow(&all, FileCap::Browse));
        assert!(caps_allow(&all, FileCap::Download));
        assert!(caps_allow(&all, FileCap::Upload));
        let none = caps(false, false, false, false);
        assert!(!caps_allow(&none, FileCap::Browse));
        assert!(!caps_allow(&none, FileCap::Download));
        assert!(!caps_allow(&none, FileCap::Upload));
        // The view-only preset: browse and download on, upload off.
        let view_only = caps(false, true, true, false);
        assert!(caps_allow(&view_only, FileCap::Browse));
        assert!(caps_allow(&view_only, FileCap::Download));
        assert!(!caps_allow(&view_only, FileCap::Upload));
    }
}
