//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan admin crate (ANT-662 Phase 2 / Candidate C, ANT-770).
//!
//! Server composition: the admin API, the security-headers/CORS layer, the
//! axum router ([`app`]), and the TLS serve loop ([`serve`]).

pub mod admin;
pub mod backup_manager;
pub mod app;
pub mod security;
pub mod serve;
