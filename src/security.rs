//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Security response headers and the CORS layer.
//!
//! Ports the `_SECURITY_HEADERS` dict and the `add_security_headers`
//! middleware in `server/api`. Applied to every response. The default CSP
//! is strict (no `unsafe-inline`); pages that need inline script/style
//! override it with their own `Content-Security-Policy` header (the
//! collaboration room, the public password page, and the reverse-proxied
//! session page).
//!
//! ANT-653 adds a CORS layer in front of the security headers: in loose
//! mode (the default) the server echoes the request `Origin` into
//! `Access-Control-Allow-Origin` (with `Access-Control-Allow-Credentials:
//! true`) and answers OPTIONS preflights, and the three
//! cross-origin-relevant security headers are relaxed, so a page served
//! from a different origin — e.g. the same server reached via a different
//! hostname, IP address, or port — can call the API with the login cookie
//! (which the auth layer issues as `SameSite=None; Secure` in loose mode).
//! Strict mode reproduces the pre-ANT-653 header set exactly.

use axum::extract::{Request, State};
use axum::http::header;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::settings::CorsMode;
use crate::state::AppState;

/// The default (strict) security headers applied to every response.
pub const SECURITY_HEADERS: [(&str, &str); 8] = [
    (
        "Content-Security-Policy",
        "default-src 'self'; \
         script-src 'self' blob:; \
         worker-src 'self' blob:; \
         style-src 'self'; \
         img-src 'self' data: blob: https://raw.githubusercontent.com https://github.com; \
         frame-src 'self'; \
         connect-src 'self'; \
         frame-ancestors 'self'; \
         base-uri 'self'; \
         form-action 'self'",
    ),
    ("Strict-Transport-Security", "max-age=63072000; includeSubDomains"),
    ("X-Content-Type-Options", "nosniff"),
    // SAMEORIGIN (not DENY) because the main app iframes the session page.
    ("X-Frame-Options", "SAMEORIGIN"),
    ("Permissions-Policy", "camera=(), microphone=(), geolocation=(), payment=()"),
    ("Cross-Origin-Embedder-Policy", "require-corp"),
    ("Cross-Origin-Opener-Policy", "same-origin"),
    ("Cross-Origin-Resource-Policy", "same-origin"),
];

/// The security headers that differ between strict and loose CORS modes
/// (ANT-653). In loose mode these replace the same-named entries of
/// [`SECURITY_HEADERS`]; everything else is identical in both modes.
///
/// - `Content-Security-Policy`: `connect-src` is widened from `'self'` to
///   `'self' https: http: ws: wss:` so a cross-origin page can XHR/WebSocket
///   to the API (the E2EE handshake remains the trust anchor for the data
///   itself).
/// - `Cross-Origin-Opener-Policy`: `same-origin-allow-popups` instead of
///   `same-origin`, so a cross-origin page keeps its window context when it
///   opens popups (e.g. app launches) instead of them being isolated.
/// - `Cross-Origin-Resource-Policy`: `cross-origin` instead of
///   `same-origin`, so the cross-origin page can load our resources.
const LOOSE_SECURITY_HEADERS: [(&str, &str); 3] = [
    (
        "Content-Security-Policy",
        "default-src 'self'; \
         script-src 'self' blob:; \
         worker-src 'self' blob:; \
         style-src 'self'; \
         img-src 'self' data: blob: https://raw.githubusercontent.com https://github.com; \
         frame-src 'self'; \
         connect-src 'self' https: http: ws: wss:; \
         frame-ancestors 'self'; \
         base-uri 'self'; \
         form-action 'self'",
    ),
    ("Cross-Origin-Opener-Policy", "same-origin-allow-popups"),
    ("Cross-Origin-Resource-Policy", "cross-origin"),
];

/// The looser CSP used for the reverse-proxied session page (noVNC and other
/// third-party app-container HTML that may rely on inline script/style).
/// Mirrors `_SESSION_CSP` in `server/api`.
///
/// `data:` is allowed in `script-src`/`worker-src` because the selkies
/// app-container loads its Playback AudioWorklet from a `data:text/javascript`
/// URL (ANT-537); without it the browser blocks the worklet and in-container
/// audio dies. The page already allows `'unsafe-inline'`, so a `data:` script
/// adds no real capability.
pub const SESSION_CSP: &str = "default-src 'self'; \
     script-src 'self' 'unsafe-inline' blob: data:; \
     worker-src 'self' blob: data:; \
     style-src 'self' 'unsafe-inline'; \
     img-src 'self' data: blob: https://raw.githubusercontent.com https://github.com; \
     frame-src 'self'; \
     connect-src 'self'; \
     frame-ancestors 'self'; \
     base-uri 'self'; \
     form-action 'self'";

/// A strict, per-response CSP with a nonce, for the collaboration room and
/// the public password page.
pub fn strict_csp(nonce: &str) -> String {
    format!(
        "default-src 'self'; \
         script-src 'self' 'nonce-{nonce}' blob:; \
         worker-src 'self' 'nonce-{nonce}' blob:; \
         style-src 'self' 'nonce-{nonce}'; \
         img-src 'self' data: blob:; \
         connect-src 'self' ws: wss:; \
         frame-ancestors 'self'; \
         base-uri 'self'; \
         form-action 'self'"
    )
}

/// Methods and request headers advertised on preflight responses.
const ALLOWED_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";
const ALLOWED_HEADERS: &str = "Authorization, Content-Type, X-Session-ID";
const PREFLIGHT_MAX_AGE: &str = "86400";

/// The security header set for a CORS mode: exactly [`SECURITY_HEADERS`] in
/// strict mode; in loose mode the same set with the three
/// cross-origin-relevant entries replaced by [`LOOSE_SECURITY_HEADERS`].
fn headers_for(mode: CorsMode) -> Vec<(&'static str, &'static str)> {
    let mut headers: Vec<(&'static str, &'static str)> = SECURITY_HEADERS.to_vec();
    if mode == CorsMode::Loose {
        for entry in &mut headers {
            if let Some(replacement) = LOOSE_SECURITY_HEADERS
                .iter()
                .find(|(name, _)| *name == entry.0)
            {
                *entry = *replacement;
            }
        }
    }
    headers
}

/// Middleware that adds the mode's security headers to every response.
///
/// Mirrors the Python `response.headers.setdefault(header, value)`: a header
/// is only added when the handler did not already set it. This lets the
/// collaboration room, the public password page, and the reverse-proxied
/// session page override the default CSP with their own (strict / nonce /
/// loose) `Content-Security-Policy`. The effective mode is resolved per
/// request from the `app_settings` table / `VRECKAN_CORS_MODE` (ANT-653).
pub async fn add_security_headers(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let cors = state.settings.cors_config(&state.db).await;
    let mut resp = next.run(req).await;
    for (name, value) in headers_for(cors.mode) {
        if let Ok(header_name) = header::HeaderName::from_bytes(name.as_bytes()) {
            if let Ok(header_value) = value.parse() {
                // `setdefault` semantics: only insert when absent.
                if resp.headers().get(&header_name).is_none() {
                    resp.headers_mut().insert(header_name, header_value);
                }
            }
        }
    }
    resp
}

/// Whether an `Origin` is accepted by the effective configuration (ANT-653),
/// and the value that should be echoed in `Access-Control-Allow-Origin`:
/// the request origin as-is when no allowlist is set, or the allowlist
/// entry's canonical form when one matches (comparison is
/// case-insensitive). `None` means the origin is not allowed.
fn allowed_origin(cors: &crate::settings::CorsConfig, origin: &str) -> Option<String> {
    if cors.allowed_origins.is_empty() {
        return Some(origin.to_string());
    }
    let lower = origin.trim().to_ascii_lowercase();
    cors.allowed_origins
        .iter()
        .find(|o| o.trim().to_ascii_lowercase() == lower)
        .cloned()
}

/// The CORS layer (ANT-653). Runs inside the security-headers middleware so
/// that preflight short-circuits still leave the full security header set on
/// the response.
///
/// Loose mode: a request carrying an `Origin` that the configuration
/// accepts gets `Access-Control-Allow-Origin` (echoing the origin, or the
/// allowlist entry's canonical form), `Access-Control-Allow-Credentials:
/// true`, and `Vary: Origin`; an OPTIONS preflight (Origin +
/// `Access-Control-Request-Method`) is answered directly with `204 No
/// Content` and the advertised methods/headers, so it never reaches a
/// route that would otherwise reject the unauthenticated request (e.g. the
/// E2EE body gate). Strict mode: no CORS headers at all — requests,
/// including preflights, flow through to the routes unchanged.
pub async fn cors(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let cors = state.settings.cors_config(&state.db).await;
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // A preflight is an OPTIONS request carrying an Origin and the
    // Access-Control-Request-Method header a browser always sends with one.
    let is_preflight = req.method() == Method::OPTIONS
        && origin.is_some()
        && req.headers().get(header::ACCESS_CONTROL_REQUEST_METHOD).is_some();

    if is_preflight {
        // `is_preflight` implies an Origin was present.
        let origin = origin.expect("preflight has an Origin");
        if cors.mode == CorsMode::Loose {
            if let Some(allow) = allowed_origin(&cors, &origin) {
                let mut resp = (StatusCode::NO_CONTENT, [(header::VARY, "Origin")]).into_response();
                let h = resp.headers_mut();
                h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow.parse().unwrap());
                h.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true".parse().unwrap());
                h.insert(
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    ALLOWED_METHODS.parse().unwrap(),
                );
                h.insert(
                    header::ACCESS_CONTROL_ALLOW_HEADERS,
                    ALLOWED_HEADERS.parse().unwrap(),
                );
                h.insert(
                    header::ACCESS_CONTROL_MAX_AGE,
                    PREFLIGHT_MAX_AGE.parse().unwrap(),
                );
                return resp;
            }
        }
        // Strict mode (or an unlisted origin): the preflight falls through
        // to the route, which handles (or rejects) it itself.
        return next.run(req).await;
    }

    let mut resp = next.run(req).await;
    if let Some(origin) = &origin {
        if cors.mode == CorsMode::Loose {
            if let Some(allow) = allowed_origin(&cors, origin) {
                let h = resp.headers_mut();
                h.insert(
                    header::ACCESS_CONTROL_ALLOW_ORIGIN,
                    allow.parse().unwrap(),
                );
                h.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true".parse().unwrap());
                match h.get(header::VARY).and_then(|v| v.to_str().ok()) {
                    Some(existing) if existing.split(',').any(|p| p.trim() == "Origin") => {}
                    Some(existing) => {
                        h.insert(header::VARY, format!("{existing}, Origin").parse().unwrap());
                    }
                    None => {
                        h.insert(header::VARY, "Origin".parse().unwrap());
                    }
                }
            }
        }
    }
    resp
}
