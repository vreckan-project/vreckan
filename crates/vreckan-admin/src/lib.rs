//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan admin crate (ANT-662 Phase 2 / Candidate C, ANT-770).
//!
//! Server composition: the admin API, the axum router ([`app`]), and the
//! TLS serve loop ([`serve`]). The security-headers/CORS layer lives in
//! [`vreckan_core::security`] (moved there in ANT-810 so the per-crate test
//! harnesses can apply the same middleware without linking this crate) and
//! is re-exported here so the pre-split `security::` paths keep resolving.

pub mod admin;
pub mod backup_manager;
pub mod app;
pub mod serve;

pub use vreckan_core::security;
