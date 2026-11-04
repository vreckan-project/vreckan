//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan collaboration crate (ANT-662 Phase 3 / Candidate B, ANT-812).
//!
//! The shared-room domain: the collaboration WebSocket service, its token
//! tables (plain-session and E2EE), and the per-room connection registry.
//! The lowest crate in the former `vreckan-apps` central cluster — it
//! depends only on `vreckan-core` (the `state`-hub cycles that made it
//! cyclic with `catalog` were broken by moving the shared
//! `random_token`/`save_sessions` helpers into core).

pub mod collaboration;
