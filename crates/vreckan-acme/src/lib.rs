//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan ACME/TLS crate (ANT-662 Phase 2 / Candidate C, ANT-770).
//!
//! Let's Encrypt issuance (the native [`acme`] client + the
//! [`acme_orchestrator`] renewal job), TLS certificate selection
//! ([`tls`]), and the E2EE handshake ([`e2ee`]).

pub mod acme;
pub mod acme_orchestrator;
pub mod e2ee;
pub mod tls;
