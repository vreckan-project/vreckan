//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan server library (Rust port) — compatibility facade.
//!
//! The application is split into a library and a thin binary (`main.rs`) so
//! the integration test suite can import the modules directly, mirroring the
//! Python layout where `tests/` imports `server.*`.
//!
//! Since the Candidate C split (ANT-662 Phase 2, ANT-770) the modules live in
//! five domain crates (`vreckan-core`, `vreckan-acme`, `vreckan-auth`,
//! `vreckan-apps`, `vreckan-admin`). This library re-exports every former
//! top-level module under its original name, so the public `vreckan` package
//! stays source-compatible for dependents: `use vreckan::catalog::...` and
//! friends resolve exactly as before. Types that moved into
//! `vreckan_core::shared` are referenced at their new paths.

pub use vreckan_admin::admin;
pub use vreckan_admin::app;
pub use vreckan_admin::serve;
pub use vreckan_acme::acme;
pub use vreckan_acme::acme_orchestrator;
pub use vreckan_acme::e2ee;
pub use vreckan_acme::tls;
pub use vreckan_apps::backup_manager;
pub use vreckan_apps::catalog;
pub use vreckan_apps::collaboration;
pub use vreckan_apps::docker_provider;
pub use vreckan_apps::files;
pub use vreckan_apps::session_proxy;
pub use vreckan_apps::session_shares;
pub use vreckan_auth::auth;
pub use vreckan_auth::user_manager;
pub use vreckan_core::db;
pub use vreckan_core::entities;
pub use vreckan_core::permissions;
pub use vreckan_core::security;
pub use vreckan_core::settings;
pub use vreckan_core::state;
pub use vreckan_core::static_files;
pub use vreckan_core::tokens;
