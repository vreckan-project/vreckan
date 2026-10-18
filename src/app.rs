//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! The axum application: router construction, middleware, and the skeleton
//! routes (health, index, static files).
//!
//! Ports the `api_app = FastAPI(...)` factory, the security-headers
//! middleware, the `read_root` handler, and the `serve_static` catch-all from
//! `server/api`. The remaining routers (auth, encrypted, session proxy,
//! collaboration, admin, ...) are mounted here as they are ported.

use axum::extract::{Path, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{Html, IntoResponse};
use axum::routing::{any, get};
use axum::Router;

use crate::admin;
use crate::auth;
use crate::catalog;
use crate::collaboration;
use crate::e2ee;
use crate::files;
use crate::security;
use crate::session_proxy;
use crate::session_shares;
use crate::state::AppState;
use crate::static_files;

/// Build the application router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        // E2EE handshake (plaintext: it proves server identity, not user
        // identity, so it cannot be gated by the session it establishes).
        .nest("/api/handshake", e2ee::handshake_router())
        // Auth routes (login/logout/me/passwords/prefs + OIDC/SSO). Mounted
        // before the static catch-all so they take precedence.
        .nest("/api/auth", auth::auth_router())
        // E2EE-encrypted catalog + launch + session routes.
        .nest("/api", catalog::catalog_router())
        // Session reverse proxy (HTTP + WebSocket) to the app containers.
        .nest("/api/apps/session", session_proxy::session_proxy_router())
        // E2EE-encrypted file-management routes (files/upload/homedirs/pinned).
        .nest("/api", files::files_router())
        // Admin management endpoints (roster, settings, apps, sessions, ...).
        .nest("/api/admin", admin::admin_router())
        // Plaintext public-share download routes.
        .merge(files::public_router())
        // E2EE session-share management API (mirrors the catalog session routes).
        .nest("/api", session_shares::owner_router())
        // Plaintext, account-less session-share public entry.
        .merge(session_shares::public_router())
        // Collaboration room (page, static assets, room WebSocket).
        .merge(collaboration::room_router())
        // Registered last so it never shadows explicit API/session routes.
        .route("/{*path}", any(serve_static))
        .layer(middleware::from_fn(security::add_security_headers))
        .with_state(state)
}

/// Serve `index.html` with the JS cache-busting version injected.
async fn index() -> impl IntoResponse {
    match static_files::index_html() {
        Some(html) => (
            [(header::CACHE_CONTROL, "no-cache")],
            Html(html),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "<h1>Vreckan Server</h1>").into_response(),
    }
}

/// Liveness probe.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let sessions = state.sessions.read().await.len();
    (StatusCode::OK, format!("ok ({} session(s))", sessions))
}

/// Catch-all static asset handler.
async fn serve_static(Path(path): Path<String>) -> impl IntoResponse {
    static_files::serve(&path)
}
