//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Static file serving for the web client.
//!
//! Ports the `serve_static` handler and `_client_js_version` helper in
//! `server/api`. The web client is framework-free JS + i18n bundles served
//! from the `server/static` directory. JS/CSS are served `no-store` (browsers
//! keep ES modules in a separate in-memory cache that ordinary cache headers
//! do not clear); other assets revalidate (`no-cache`).

use axum::http::header;
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
