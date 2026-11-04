//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Vreckan catalog crate (ANT-662 Phase 3 / Candidate B, ANT-812).
//!
//! The app domain: the installed-app catalog, app stores and templates,
//! image pull/update machinery, app launches, and the Docker Engine API
//! layer (`docker_provider`) the launches run through. Sits above
//! `vreckan-sessions` and `vreckan-collab` in the crate DAG; the shared
//! `random_token`/`random_uuid`/`save_sessions`/`load_sessions` helpers it
//! once lent to the other modules now live in `vreckan-core`, which is
//! what broke the old in-crate cycles.

pub mod catalog;
pub mod docker_provider;
