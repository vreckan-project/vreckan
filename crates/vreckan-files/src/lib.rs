//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan files crate (ANT-662 Phase 3 / Candidate B, ANT-812).
//!
//! The files domain: chunked uploads, file storage, and public shares
//! (password-protected or open download links). The top of the former
//! `vreckan-apps` central cluster; its one cross-domain edges point
//! downward (`catalog` for the launch/upload machinery, `sessions` for the
//! expired-share housekeeping). The password page it once lent to
//! `session_shares` now lives in `core::static_files`.

pub mod files;
