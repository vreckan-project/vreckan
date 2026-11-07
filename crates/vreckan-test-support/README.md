# vreckan-test-support

Shared integration-test harness for the vreckan domain crates. Extracted in
ANT-853 (ANT-849 #4) from the five drifted per-crate
`tests/common/mod.rs` copies created in ANT-810 (2,922 lines of
mostly-duplicated plumbing).

Dev-only: a `dev-dependency` of `vreckan-admin`, `vreckan-auth`,
`vreckan-catalog`, `vreckan-files`, and `vreckan-sessions`; never linked by
production code.

## Shared plumbing (in `src/lib.rs`)

`DEAD_URL`, `test_settings`, `connect_db`, `build_state_base` (throwaway
dirs + fresh SQLite DB + `AppState::new`, no lifespan seeding),
`compose_router` (the router frame mirroring `vreckan_admin::app::router`:
index/health, E2EE handshake, auth routes, static catch-all,
security-headers/CORS layers; the per-crate domain routes are spliced in by
a closure before the layers/state binding), `RawResponse`, `TestClient`,
`SecureClient` (login + E2EE handshake + AES-GCM call crypto), `TempDir`,
and the `rand::Rng` re-export.

`http-body-util` (used only by the `TestClient`'s oneshot request bodies)
moved into this crate from the root `vreckan` package's `[dependencies]` in
ANT-853.

## Per-crate deltas (kept in each crate's `tests/common.rs` shim)

Each of the five domain crates keeps a thin `tests/common.rs` that re-exports
this crate (so the test files' `mod common;` / `use common::{...}` call
sites are unchanged) and holds the crate-local parts:

| crate | router composition | state seeding on top of `build_state_base` |
|---|---|---|
| vreckan-auth | shared frame, no extra domain routes | users/groups + E2EE server key; **skips the catalog warm-up** |
| vreckan-sessions | + catalog, session-proxy, session-share, collaboration routes | users/groups + E2EE key + catalog warm-up |
| vreckan-catalog | + catalog, session-proxy, session-share, collaboration routes | users/groups + E2EE key + catalog warm-up |
| vreckan-files | + files, public-share routes | users/groups + E2EE key + catalog warm-up |
| vreckan-admin | the real app router (`vreckan_admin::app::router`) | users/groups + E2EE key + catalog warm-up (complete lifespan) |

The shims also wrap `TestClient`/`SecureClient` (newtype + `Deref`/
`DerefMut`) so `TestClient::new(state)` / `SecureClient::new(state)` build
clients on the crate's own router.

## Link closure

This crate depends only on `vreckan-core`, `vreckan-acme`, and
`vreckan-auth` — the three crates every one of the five domain crates
already links — so dev-depending on it introduces no new link closure and
keeps the admin crate (and, transitively, the whole program) out of the
subset-router test binaries.
