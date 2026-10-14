//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan server library (Rust port).
//!
//! The application is split into a library (this crate) and a thin binary
//! (`main.rs`) so the integration test suite can import the modules directly,
//! mirroring the Python layout where `tests/` imports `server.*`.

pub mod acme;
pub mod admin;
pub mod app;
pub mod auth;
pub mod backup_manager;
pub mod catalog;
pub mod collaboration;
pub mod db;
pub mod docker_provider;
pub mod e2ee;
pub mod entities;
pub mod files;
pub mod le;
pub mod permissions;
pub mod security;
pub mod session_proxy;
pub mod settings;
pub mod state;
pub mod static_files;
pub mod tls;
pub mod user_manager;
