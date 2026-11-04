//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Static file serving for the web client.
//!
//! Ports the `serve_static` handler and `_client_js_version` helper in
//! `server/api`. The web client is framework-free JS + i18n bundles served
//! from the `server/static` directory. JS/CSS are served `no-store` (browsers
//! keep ES modules in a separate in-memory cache that ordinary cache headers
//! do not clear); other assets revalidate (`no-cache`).

use axum::http::header;
use base64::Engine;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// The static directory, overridable via `VRECKAN_STATIC_DIR`.
pub fn static_dir() -> PathBuf {
    std::env::var("VRECKAN_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("server/static"))
}

/// A short content hash over every file under `static/js`, used to cache-bust
/// the module script tag in `index.html`. Editing any client script bumps the
/// version and forces a fresh fetch.
pub fn client_js_version() -> String {
    let js_dir = static_dir().join("js");
    let mut digest = Sha256::new();
    if let Ok(entries) = std::fs::read_dir(&js_dir) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        for name in names {
            if name.ends_with(".js") {
                let path = js_dir.join(&name);
                if let Ok(data) = std::fs::read(&path) {
                    digest.update(name.as_bytes());
                    digest.update(&data);
                }
            }
        }
    }
    let hex = hex::encode(digest.finalize());
    hex[..16.min(hex.len())].to_string()
}

/// Read `index.html`, inject the JS cache-busting version, and return it with
/// a `no-cache` header so a new deploy is picked up on the next load.
pub fn index_html() -> Option<String> {
    let path = static_dir().join("index.html");
    let mut html = std::fs::read_to_string(path).ok()?;
    let version = client_js_version();
    html = html.replace(
        "src=\"/js/entry.js\"",
        &format!("src=\"/js/entry.js?v={version}\""),
    );
    Some(html)
}

/// Serve a single static asset, guarding against path traversal.
pub fn serve(path: &str) -> Response {
    let base = static_dir();
    let base_canon = base.canonicalize().unwrap_or_else(|_| base.clone());
    let candidate = base.join(path);
    let candidate_canon = candidate.canonicalize().unwrap_or_else(|_| candidate.clone());

    // Only files that physically exist under the static directory are served.
    if candidate_canon != base_canon && !candidate_canon.starts_with(&base_canon) {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }
    if !candidate_canon.is_file() {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }

    let data = match std::fs::read(&candidate_canon) {
        Ok(d) => d,
        Err(_) => return (StatusCode::NOT_FOUND, "Not found").into_response(),
    };

    let mime = mime_guess::from_path(candidate_canon.to_string_lossy().as_ref())
        .first_or_octet_stream()
        .to_string();
    let lower = candidate_canon.to_string_lossy().to_lowercase();
    let cache_control = if lower.ends_with(".js") || lower.ends_with(".css") {
        "no-store"
    } else {
        "no-cache"
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache_control)
        .body(axum::body::Body::from(data))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

/// Render the public password page with template substitution + strict CSP.
/// `action_url` is where the form POSTs (the share's public entry point —
/// `/public/{share_id}` for file shares, `/share/{share_id}` for session
/// shares). `status` is the HTTP status to return: 200 when the page is
/// presented on a GET (the share is password-protected), 401 when a POST
/// submitted a wrong password (mirrors the Python `access_public_share_post`).
///
/// Moved here from `files` in the ANT-812 central-cluster split: the session
/// share handlers rendered it and the files routes consumed a function from
/// the session-share module in return, which kept `files` and
/// `session_shares` cyclic; this page only needs the static template and the
/// shared CSP helper, both core.
pub fn render_password_page(
    action_url: &str,
    share_id: &str,
    error_message: &str,
    status: StatusCode,
) -> Response {
    let template_path = static_dir().join("public_password.html");
    let nonce = {
        let mut buf = [0u8; 16];
        use rand::Rng;
        rand::rng().fill_bytes(&mut buf);
        base64::engine::general_purpose::STANDARD.encode(buf)
    };
    let error_html = if error_message.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div class="error-message" role="alert" style="margin-top:1rem;font-size:0.9rem;color:var(--color-danger);text-align:center;">{error_message}</div>"#
        )
    };
    let html = match std::fs::read_to_string(&template_path) {
        Ok(t) => t
            .replace("{{ACTION_URL}}", action_url)
            .replace("{{SHARE_ID}}", share_id)
            .replace("{{ERROR_MESSAGE}}", &error_html)
            .replace("{{CSP_NONCE}}", &nonce),
        Err(_) => "<h1>Password protected</h1>".to_string(),
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_SECURITY_POLICY, crate::shared::strict_csp(&nonce))
        .body(axum::body::Body::from(html))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}
