//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan server entry point (Rust port).
//!
//! Ports `run_https`: ensure the data directories exist, select the TLS
//! cert (Let's Encrypt or self-signed), build the application state, and
//! serve the axum app over TLS on `0.0.0.0:$VRECKAN_API_PORT`.

use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::info;

use vreckan::{app, auth, catalog, db, e2ee, files, le_certbot, tls, user_manager};
use vreckan::settings::Settings;
use vreckan::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Structured logging, honoring RUST_LOG (falls back to the configured
    // log level).
    let log_level = std::env::var("VRECKAN_LOG_LEVEL")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&log_level)),
        )
        .init();

    let settings = Settings::from_env();

    // P2-6 (ANT-90): guard against the bundled Postgres' default vreckan/
    // vreckan credentials before connecting (see `uses_default_postgres_creds`
    // and `default_db_creds_policy`). Runs before any DB/schema work so a
    // `Refuse` policy fails fast on the startup path.
    if settings.uses_default_postgres_creds() {
        match vreckan::settings::default_db_creds_policy() {
            vreckan::settings::DefaultDbCredsPolicy::Silent => {}
            vreckan::settings::DefaultDbCredsPolicy::Warn => tracing::warn!(
                "using the bundled Postgres default credentials vreckan/vreckan; \
                 set VRECKAN_DB_PASS (or VRECKAN_DATABASE_URL) to change them, or \
                 VRECKAN_ALLOW_DEFAULT_DB_CREDS=1 to silence this (ANT-45 P2-6)"
            ),
            vreckan::settings::DefaultDbCredsPolicy::Refuse => {
                return Err(anyhow::anyhow!(
                    "the server is using the bundled Postgres default credentials \
                     (vreckan/vreckan) and VRECKAN_ALLOW_DEFAULT_DB_CREDS=refuse \
                     is set; set a real password, or set \
                     VRECKAN_ALLOW_DEFAULT_DB_CREDS=1 to start anyway (ANT-45 P2-6)"
                ))
            }
        }
    }

    ensure_data_dirs(&settings);

    // Load (or generate on first run) the E2EE server RSA key. Mirrors the
    // `api` import-time key load; it must exist before any handshake.
    e2ee::init_server_key(&settings)?;

    // Select the rustls crypto provider (ring) before any TLS config is built.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Initialise the configuration database (creates tables on first run) and
    // apply any idempotent column migrations.
    let db = db::connect(&settings.build_database_url())
        .await
        .map_err(anyhow::Error::msg)?;
    db.init().await.map_err(anyhow::Error::msg)?;

    // Build the shared application state (needed by cert selection so the
    // effective LE settings can be read).
    let state = AppState::new(settings, db);

    // TLS cert selection (Let's Encrypt or self-signed fallback).
    let (cert_pem, key_pem) = tls::select_cert(&state).await?;

    // Load the durable stores from the database (mirrors the Python `lifespan`
    // startup). Auth tokens first, then users/groups/roles (the latter also
    // runs the first-run bootstrap and idempotent migrations).
    auth::load_auth_tokens(&state)
        .await
        .map_err(anyhow::Error::msg)?;
    user_manager::load_users_and_groups(&state.db, &state.settings)
        .await
        .map_err(anyhow::Error::msg)?;

    // Catalog startup (mirrors the Python `lifespan` catalog section): the
    // editable template schema, persisted sessions, the self-container
    // inspection (path-prefix map + network discovery), the installed apps and
    // app stores, the app templates, and GPU detection.
    catalog::load_template_schema(&state).await;
    catalog::load_sessions(&state).await;
    catalog::inspect_self_container(&state).await;
    catalog::load_installed_apps(&state).await;
    catalog::load_app_stores(&state).await;
    catalog::load_app_templates(&state).await;
    catalog::detect_gpus(&state).await;
    files::load_public_shares(&state).await;

    // Let's Encrypt renewal: started unconditionally; the job no-ops (sleeps)
    // when LE is disabled or auto-renew is off. Mirrors the Python lifespan.
    tokio::spawn(le_certbot::background_le_renewal_job(state.clone()));

    // Build the router.
    let app = app::router(state);

    let port = std::env::var("VRECKAN_API_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(443);
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;

    info!("vreckan (rust) listening on https://{addr}");

    let config = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .map_err(anyhow::Error::msg)?;
    axum_server::tls_rustls::bind_rustls(addr, config)
        // ConnectInfo exposes the client address to handlers (the login
        // rate limiter keys on it); `X-Forwarded-For` still takes precedence
        // when a reverse proxy is in front.
        .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await
        .map_err(anyhow::Error::msg)?;

    Ok(())
}

/// Create the persistent data directories, mirroring `run_https`.
fn ensure_data_dirs(settings: &Settings) {
    let work = PathBuf::from(&settings.data_root);
    for sub in ["storage", "ssl", "autostart_cache", "app_stores_cache", "backups"] {
        std::fs::create_dir_all(work.join(sub)).ok();
    }
    for sub in [
        "vreckan_app_icons",
        "vreckan_home_templates",
        "vreckan_public",
        "vreckan_uploads",
    ] {
        std::fs::create_dir_all(work.join("storage").join(sub)).ok();
    }
}
