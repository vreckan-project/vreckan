//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan server entry point (Rust port).
//!
//! Ports `run_https`: ensure the data directories exist, select the TLS
//! cert (Let's Encrypt or self-signed), build the application state, and
//! serve the axum app over TLS on `0.0.0.0:$VRECKAN_API_PORT`.

use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::info;

use vreckan::{
    acme_orchestrator, app, auth, catalog, db, e2ee, files, session_shares, tls, user_manager,
};
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
        match settings.default_db_creds_policy {
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
    let mut state = AppState::new(settings, db);

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
    // Warm the in-memory image-metadata cache so the installed-apps table
    // shows each image's real identity right after a restart instead of
    // "unknown" (ANT-266/ANT-269). Best-effort: a down Docker daemon is
    // logged and the apps stay "unknown"; never fails startup.
    catalog::warm_image_metadata(&state).await;
    catalog::load_app_stores(&state).await;
    catalog::load_app_templates(&state).await;
    catalog::detect_gpus(&state).await;
    files::load_public_shares(&state).await;
    session_shares::load_session_shares(&state).await;

    // Build the TLS listener config before the renewal job starts, so both the
    // background renewal job and the admin Issue/Renew path can reload it in
    // place. axum-server's `RustlsConfig` is an `Arc<ArcSwap<ServerConfig>>`:
    // a renewal stores a fresh `ServerConfig` and the *next* TLS handshake
    // serves it — no process restart, no port rebind. `state.tls_config` holds
    // a shared clone the ACME paths use to trigger the swap.
    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .map_err(anyhow::Error::msg)?;
    state.tls_config = Some(tls_config.clone());

    // ACME renewal: started unconditionally; the job no-ops (sleeps) when ACME
    // is disabled or auto-renew is off. On a successful renewal it hot-reloads
    // the served cert in place. Mirrors the Python lifespan.
    tokio::spawn(acme_orchestrator::background_acme_renewal_job(state.clone()));

    // App auto-update: started unconditionally; each tick re-reads
    // `auto_update_apps` / `auto_update_interval_seconds` fresh (admin-editable,
    // no restart needed) and no-ops when auto-update is disabled. The first
    // tick runs immediately at startup: it re-warms the local image metadata
    // and compares every installed image against its registry, auto-pulling
    // opted-in apps that have an available update (ANT-269).
    tokio::spawn(catalog::background_auto_update_job(state.clone()));

    // Share cleanup (docs/port-status.md G2): sweeps expired file shares and
    // expired session shares (and their per-visit tokens). The first tick
    // runs immediately at startup; the interval is re-read fresh each tick.
    tokio::spawn(files::background_share_cleanup_job(state.clone()));

    // Build the router.
    let app = app::router(state);

    let port = std::env::var("VRECKAN_API_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(443);
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;

    info!("vreckan (rust) listening on https://{addr}");

    axum_server::tls_rustls::bind_rustls(addr, tls_config)
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
