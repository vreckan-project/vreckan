//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Security response headers.
//!
//! Ports the `_SECURITY_HEADERS` dict and the `add_security_headers`
//! middleware in `server/api`. Applied to every response. The default CSP
//! is strict (no `unsafe-inline`); pages that need inline script/style
//! override it with their own `Content-Security-Policy` header (the
//! collaboration room, the public password page, and the reverse-proxied
//! session page).

use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;

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
         worker-src 'self' blob:; \
         style-src 'self' 'nonce-{nonce}'; \
         img-src 'self' data: blob:; \
         connect-src 'self' ws: wss:; \
         frame-ancestors 'self'; \
         base-uri 'self'; \
         form-action 'self'"
    )
}

/// Middleware that adds the default security headers to every response.
///
/// Mirrors the Python `response.headers.setdefault(header, value)`: a header
/// is only added when the handler did not already set it. This lets the
/// collaboration room, the public password page, and the reverse-proxied
/// session page override the default CSP with their own (strict / nonce /
/// loose) `Content-Security-Policy`.
pub async fn add_security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    for (name, value) in SECURITY_HEADERS {
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
