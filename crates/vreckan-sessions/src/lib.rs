//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan sessions crate (ANT-662 Phase 3 / Candidate B, ANT-812).
//!
//! The session domains: session shares (join grants, visit tokens, public
//! share links) and the selkies session proxy (the per-session reverse
//! proxy in front of the app containers' WebRTC/signaling endpoints).
//! Sits below `vreckan-catalog` and above `vreckan-collab` in the crate
//! DAG; the shared `random_uuid`/`random_token`/`save_sessions` helpers it
//! once pulled from `catalog` now live in `vreckan-core`, and the password
//! page it rendered from `files` now lives in `core::static_files`.

pub mod session_proxy;
pub mod session_shares;
