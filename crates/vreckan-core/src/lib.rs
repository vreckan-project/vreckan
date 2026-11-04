//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan core crate (ANT-662 Phase 2 / Candidate C, ANT-770).
//!
//! The foundation the other domain crates build on: the SeaORM database
//! layer and entities, settings, permissions, tokens, static-file serving,
//! and the process-wide [`state::AppState`]. The [`shared`] module holds the
//! small data types that `AppState` (and a few response-header helpers)
//! reference across crate boundaries, so this crate has no edge back into
//! the domain crates.

pub mod db;
pub mod entities;
pub mod permissions;
pub mod security;
pub mod settings;
pub mod shared;
pub mod state;
pub mod static_files;
pub mod tokens;
