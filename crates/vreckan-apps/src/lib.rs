//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan apps crate (ANT-662 Phase 2 / Candidate C, ANT-770).
//!
//! The application domains: the app catalog and store, the Docker provider,
//! files and public shares, session shares and the selkies session proxy,
//! the collaboration room, and backup/restore.

pub mod backup_manager;
pub mod catalog;
pub mod collaboration;
pub mod docker_provider;
pub mod files;
pub mod session_proxy;
pub mod session_shares;
