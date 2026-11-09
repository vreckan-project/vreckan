//! Test harness for the vreckan-admin integration tests (ANT-810; the
//! shared plumbing was extracted to `vreckan-test-support` in ANT-853).
//!
//! The admin tests drive the full app router
//! (`vreckan_admin::app::router`), so this harness keeps the complete
//! lifespan seeding. Everything the five former per-crate copies shared
//! (settings, DB, the state-construction base, the router frame, the
//! HTTP/E2EE clients, TempDir) now lives in the shared
//! `vreckan-test-support` crate, re-exported below so the test files'
//! `mod common;` call sites are unchanged. What stays here is
//! crate-local: the app router and the lifespan seeding, wrapped so
//! `TestClient::new(state)` / `SecureClient::new(state)` keep their
//! signatures.
//!
//! Regression note (ANT-914): these test binaries run without a logging
//! subscriber, so `tracing` `warn!`/`error!` calls in library code are
//! invisible here. Test code must never rely on log visibility to surface
//! a failure — assert state directly, or `expect`/`unwrap` on the error
//! the code under test returns (see the `load_template_schema` seeding
//! below, whose silent early-returns masked a flaky seed).

#![allow(dead_code)]

pub use vreckan_test_support::*;

use vreckan_acme::e2ee;
use vreckan_admin::app;
use vreckan_auth::user_manager;
use vreckan_catalog::catalog;
use vreckan_core::settings::Settings;
use vreckan_core::state::AppState;

/// The axum router for a test (the real app router, state-bound).
pub fn router(state: AppState) -> axum::Router {
    app::router(state)
}

/// Build a fresh `AppState` for a test: throwaway dirs, fresh DB, bootstrap
/// admin + roles seeded (mirrors the Python lifespan startup).
pub async fn build_state(root: &std::path::Path) -> AppState {
    build_state_with(root, |_| {}).await
}

/// Like `build_state`, but lets the test mutate the `Settings` before the
/// state is built (e.g. to flip a policy flag). The shared half of the
/// construction lives in `vreckan_test_support::build_state_base`; the
/// lifespan seeding below is crate-local (ANT-853).
pub async fn build_state_with(
    root: &std::path::Path,
    mutate: impl FnOnce(&mut Settings),
) -> AppState {
    let state = vreckan_test_support::build_state_base(root, mutate).await;
    // Mirror the lifespan startup: bootstrap admin + roles, then load the
    // durable stores.
    user_manager::load_users_and_groups(&state.db, &state.settings)
        .await
        .expect("load users/groups");
    // Initialise the E2EE server key (a process-wide OnceLock, so only the
    // first call actually loads/generates it).
    e2ee::init_server_key(&state.settings).expect("init server key");
    // Seed the template schema from the bundled YAML baseline (mirrors the
    // lifespan startup). `expect` (not a swallowed warn): a transient SQLite
    // lock under parallel-suite load used to leave the initial empty schema
    // in place silently (ANT-914).
    catalog::load_template_schema(&state)
        .await
        .expect("load template schema");
    // Assert the baseline actually seeded: the bundled
    // `server/template_schema.yml` is non-empty, so an empty settings list
    // here means the seed silently no-opped. (The guard is scoped so it
    // drops before `state` is returned.)
    {
        let schema = state.template_schema.read().await;
        assert!(
            !schema["settings"]
                .as_array()
                .map(Vec::is_empty)
                .unwrap_or(true),
            "template schema did not seed from the bundled baseline"
        );
    }
    // Load application templates (built-in YAML + DB overrides; seeds a blank
    // "Default" template when none exist). Mirrors the lifespan startup.
    catalog::load_app_templates(&state).await;
    // Load the app stores (seeds the default "SealSkin Apps" store on first
    // run). Mirrors the lifespan startup.
    catalog::load_app_stores(&state)
        .await
        .expect("load app stores");
    // Load the installed apps and warm the image-metadata cache (the
    // lifespan startup path, ANT-269). Fresh test databases have no
    // installed apps yet, so the warm-up is a no-op unless a test seeds the
    // `installed_apps` table before calling `build_state_with`; the
    // `fake_provider` / `pull_noop` hooks are honored by the helpers.
    catalog::load_installed_apps(&state)
        .await
        .expect("load installed apps");
    catalog::warm_image_metadata(&state).await;
    state
}

/// A test client that drives this crate's router (see [`router`]) — wraps
/// the shared client so the `new(state)` call sites in the test files are
/// unchanged (ANT-853).
pub struct TestClient(pub vreckan_test_support::TestClient);

impl TestClient {
    pub fn new(state: AppState) -> Self {
        Self(vreckan_test_support::TestClient::new(router(state)))
    }
}

impl std::ops::Deref for TestClient {
    type Target = vreckan_test_support::TestClient;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for TestClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// A logged-in, E2EE-capable test client for this crate's router (see
/// [`router`]) — wraps the shared client so the `new(state)` call sites in
/// the test files are unchanged (ANT-853).
pub struct SecureClient(pub vreckan_test_support::SecureClient);

impl SecureClient {
    pub fn new(state: AppState) -> Self {
        Self(vreckan_test_support::SecureClient::new(
            router(state.clone()),
            state,
        ))
    }
}

impl std::ops::Deref for SecureClient {
    type Target = vreckan_test_support::SecureClient;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SecureClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
