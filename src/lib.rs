//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan server library (Rust port) — compatibility facade.
//!
//! The application is split into a library and a thin binary (`main.rs`) so
//! the integration test suite can import the modules directly, mirroring the
//! Python layout where `tests/` imports `server.*`.
//!
//! Since the Candidate C split (ANT-662 Phase 2, ANT-770) the modules live in
//! domain crates, and since the Candidate B central-cluster split (ANT-662
//! Phase 3, ANT-812) the former `vreckan-apps` crate is dissolved into
//! `vreckan-collab`, `vreckan-sessions`, `vreckan-catalog` (catalog + Docker
//! provider), and `vreckan-files`, with `backup_manager` in
//! `vreckan-admin`. This library re-exports every former top-level module
//! under its original name, so the public `vreckan` package stays
//! source-compatible for dependents: `use vreckan::catalog::...` and friends
//! resolve exactly as before. Types that moved into `vreckan_core::shared`
//! are referenced at their new paths.

pub use vreckan_admin::admin;
pub use vreckan_admin::app;
pub use vreckan_admin::security;
pub use vreckan_admin::serve;
pub use vreckan_acme::acme;
pub use vreckan_acme::acme_orchestrator;
pub use vreckan_acme::e2ee;
pub use vreckan_acme::tls;
pub use vreckan_admin::backup_manager;
pub use vreckan_catalog::catalog;
pub use vreckan_collab::collaboration;
pub use vreckan_catalog::docker_provider;
pub use vreckan_files::files;
pub use vreckan_sessions::session_proxy;
pub use vreckan_sessions::session_shares;
pub use vreckan_auth::auth;
pub use vreckan_auth::user_manager;
pub use vreckan_core::db;
pub use vreckan_core::entities;
pub use vreckan_core::permissions;
pub use vreckan_core::settings;
pub use vreckan_core::state;
pub use vreckan_core::static_files;
pub use vreckan_core::tokens;
