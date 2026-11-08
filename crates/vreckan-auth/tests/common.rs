//! Test harness for the vreckan-auth integration tests (ANT-810; the
//! shared plumbing was extracted to `vreckan-test-support` in ANT-853).
//!
//! The test router is the shared frame (`vreckan_test_support::
//! compose_router` — the same security-headers/CORS layers as the app
//! router) with no extra domain routes: the auth tests exercise only the
//! auth routes and the E2EE handshake, so the state builder seeds
//! users/groups and the E2EE server key but skips the catalog warm-up, and
//! the test binaries link `vreckan-auth` + `vreckan-acme` (handshake) +
//! `vreckan-core`, never the app-domain crates or the admin composition.
//! Everything the five former per-crate copies shared is re-exported from
//! the shared crate below so the test files' `mod common;` call sites are
//! unchanged; what stays here is crate-local: the seeding and the client
//! wrappers.

#![allow(dead_code)]

pub use vreckan_test_support::*;

use vreckan_acme::e2ee;
use vreckan_auth::user_manager;
use vreckan_core::settings::Settings;
use vreckan_core::state::AppState;

/// The axum router for a test: the shared frame (the E2EE handshake, the
/// auth routes, and the static catch-all) with no extra domain routes.
pub fn router(state: AppState) -> axum::Router {
    vreckan_test_support::compose_router(state, |r| r)
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
    // first call actually loads/generates it). The handshake the
    // `SecureClient` performs is part of the auth test flow (ANT-810), so
    // the key is seeded here; the catalog warm-up is skipped because the
    // auth tests never touch the app-store/template state.
    e2ee::init_server_key(&state.settings).expect("init server key");
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
