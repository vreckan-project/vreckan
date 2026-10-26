# Vreckan (Rust) — Port Status & Known Gaps

A **living record** of the Rust port's known gaps relative to the Python
original. Source of truth: the **ANT-5 functionality parity audit**
(2026-10-12, Rob) — static analysis of both codebases with the route tables
auto-extracted and diffed, plus handler-level verification. The audit's full
report and per-route parity matrix are attached to ANT-5.

**Baselines:** Python original — Gitea `repo.vreckanproject.com/vreckan/vreckan`
at `e7b2de6` (v0.5.0, user-confirmed canonical on ANT-1). Rust port — this
repo, `main` at `9a2a902` (initial port commit); the ANT-3 Docker/compose
follow-up does not change the endpoint surface.

## Summary status (as of the 2026-10-12 audit)

| Area | Status |
|---|---|
| Endpoint surface | **Complete** — all unique paths of the original present (106 unique paths in the current tree; see [api.md](./api.md)); representation-level differences only; 1 additive Rust endpoint (`/api/health`) |
| 13-table config schema + column migrations | ✅ matches (SeaORM schema-sync subsumes Python `init_db` + `migrate_db`) |
| E2EE handshake (RSA-PSS / RSA-OAEP / AES-GCM, AAD=session) | ✅ matches |
| Auth (cookie/Bearer tokens), OIDC provisioning + group re-stamp, RBAC | ✅ matches |
| Launch orchestration (env layering, volumes, GPU/DRI3, network, container lifecycle) | ✅ matches |
| Session persistence across restart (save/load at all mutation sites) | ✅ matches (relaunch gap: G3) |
| Backup/restore round-trip incl. chunked restore | ✅ matches |
| Public shares, pinned behaviors, homedirs, app stores/templates | ✅ matches (cleanup jobs: G2) |
| Web client (`server/static/`, 68 files) | ✅ **byte-identical** |
| Test suite | 388 original tests across 21 modules (baseline `e7b2de6`): 376 ported (367 by name, 9 renamed in the le_certbot→acme port); 12 originals without a Rust counterpart; 32 Rust-only additions (see below) |
| Open gaps | **G1–G11** below (3× P1, 3× P2, 5× P3) |

## Open gaps

Statuses are as of the audit date; update a row to **Closed (date, PR)** when
the fix ships, and append new gaps here as they are discovered (next audit
supersedes this list). Gap tracking for the core dev is coordinated through
ANT-1.

### P1 — user-visible on a regular deployment

| ID | Gap | Where (Rust) | Impact | Status |
|---|---|---|---|---|
| **G1** | **Volume-mount application at launch is a no-op.** `apply_external_mounts()` is an explicit `TODO(port admin)` placeholder on the launch path. The admin CRUD side is fully ported (routes, table, 6-field schema), so mounts can be defined but are never attached to containers | `catalog.rs` (`apply_external_mounts`, called from launch) | User/group-scoped mounts created in the admin UI persist but are silently ignored when containers launch | Open (2026-10-12) |
| **G2** | **Crypto-session sweeper still missing; the auto-update job covers only part of Python's job 1.** Python's lifespan spawns app-store/autostart cache refresh + auto-update pulls + dangling-image prune, a share-cleanup sweeper, a crypto-session sweeper, and the LE renewal job; Rust spawns the LE renewal job, the auto-update job (v0.7.0, ANT-269 — re-warms image metadata, compares registry digests, auto-pulls opted-in apps), and the session-share cleanup sweeper (ANT-305) | `main.rs` startup | The Rust crypto-session map is unbounded and stale keys stay valid indefinitely (Python: 24 h idle TTL); the auto-update job does not do the app-store/autostart cache refresh (G9) or the dangling-image prune; `share_cleanup_interval_seconds` drives the session-share sweeper | Open (2026-10-12; share sweeper landed 2026-10-17; auto-update job landed 2026-10-16, v0.7.0) |
| **G3** | **Stale-session relaunch at startup missing.** Python scans persisted sessions for missing containers and relaunches them so sessions survive a host reboot; Rust only restores session records | `main.rs` (after `load_sessions`; the `launch_kwargs` needed for relaunch are already stored) | After a reboot, restored sessions look active until a user manually recreates them | Open (2026-10-12) |

### P2 — divergence with operational consequences

| ID | Gap | Where (Rust) | Impact | Status |
|---|---|---|---|---|
| **G4** | **Template-schema startup reconciliation missing.** Python re-baselines the schema table on every start (adds YAML-defined settings missing from the table, resets drifted built-in `default` values from the YAML baseline, clears redundant `current == default`); Rust only seeds when the table is empty | `catalog.rs` (`load_template_schema`) | On a DB that predates the split, or whose baseline drifted after an image upgrade, Rust keeps serving the stale value until an admin re-saves the schema via POST | Open (2026-10-12) |
| **G5** | **No automatic env → `app_settings` mirror at startup.** Python writes every live `VRECKAN_*` env value into the `app_settings` table on start (making the DB a recovery source for the container's configuration, including the OIDC client secret); Rust only writes rows when an admin explicitly saves global settings/SSO | `main.rs` startup | Settings resolution order (DB → env → default) is preserved, but the DB no longer mirrors the env; recovery from a lost `.env` and the "UI value wins over env" audit trail are lost | Open (2026-10-12) |
| **G6** | **Collaboration-room strict CSP differs.** Rust `strict_csp` drops `style-src 'unsafe-inline'` and the `https://cdnjs.cloudflare.com` (Font Awesome) origin from style-src, drops the GitHub img-src entries, and drops `frame-src`; it adds `connect-src ws: wss:` (arguably a fix — Python's room WebSocket connected cross-scheme under a `connect-src 'self'`-only policy) | `security.rs` (used by the room page and the public password page) | Icon font resolved (ANT-570): Font Awesome 6.4.0 is vendored under `server/static/collaboration/font-awesome/` and `room.html` links the local copy, so no CSP change was needed; inline-style / GitHub img-src / `frame-src` differences remain | Partially resolved (2026-10-26, icon font via ANT-570) |

### P3 — nits, cold-start behavior, cosmetic

| ID | Gap | Where | Impact | Status |
|---|---|---|---|---|
| **G7** | Trailing-slash direction on the `/api/homedirs` and `/api/pinned` collection roots — as of the 2026-10-16 re-check (ANT-242), axum 0.8 collapses the nested `"/"` route onto the prefix, so **Rust serves the no-slash forms, the same as Python**; only the trailing-slash forms 404 (the original audit had the direction inverted) | `files.rs` route registration | Only strict clients that call the trailing-slash forms get 404; the shipped web client and the test suite both use the no-slash forms | Open (2026-10-12; direction corrected 2026-10-16) |
| **G8** | UUID path-param validation dropped on session/room paths — Python 422'd malformed ids at routing; Rust matches any string and the handler fails later (404/500) | `catalog.rs`, `session_proxy.rs`, `collaboration.rs` | Different error class/status for malformed ids | Open (2026-10-12) |
| **G9** | **Startup cache warm-up missing** — initial population of the app-store YAML caches, autostart-script caches, and image-metadata caches (Python does this in lifespan before serving) | consequence of G2 | First client requests on a cold Rust boot pay the fetch latency | Open (2026-10-12) |
| **G10** | Rust-only `GET /api/health` liveness endpoint (no Python counterpart) | `app.rs` | Additive, non-breaking; strictly an addition, listed for completeness | Additive (2026-10-12) |
| **G11** | Several Python response models (e.g. management-data, backup info, restore result, update-check responses) are inline JSON values rather than first-class Rust structs; request models are all properly typed | `admin.rs` and related handlers | Cosmetic; no behavior impact found | Open (2026-10-12) |

## Test-suite parity notes

Verified against the Python baseline (`e7b2de6`, v0.5.0) on 2026-10-16
(ANT-256): **21 Python test modules** against **21 Rust test modules**. All
21 modules have a Rust counterpart; 20 match by name, and
`test_le_certbot` became `test_acme` in the native acmex port (ANT-128),
which also renamed part of the test names. Test-level picture:

- **Ported: 376 of 388** — 367 under identical names (354 within
  same-named modules, plus 13 original `test_le_certbot` names preserved
  unchanged in `test_acme`), and 9 ported under renamed le→acme names
  (incl. `test_effective_le_config_reads_env_defaults` →
  `test_effective_acme_config_defaults`).
- **Without a Rust counterpart: 12**
  - `test_db::test_concurrent_saves_do_not_lose_data` (10 concurrent
    full-replace session saves must not lose rows) — still a real gap;
    covers the same area as G2's session handling.
  - `test_db::test_sessions_roundtrip` — covered under a different name by
    `test_postgres::test_pg_sessions_roundtrip`, so no functional gap.
    (`test_postgres` is a baseline module, not a Rust addition: all 6 of
    its tests map 1:1 by name in Rust.)
  - 10 certbot-CLI tests in `test_le_certbot` (4× `test_certbot_args_*`,
    6× `test_run_certbot_*`) — bound to the certbot subprocess the Rust
    port replaced with native acmex (ANT-128); the corresponding
    issue/renewal paths are exercised by the new `test_acme` tests.
- **Rust-only additions: 32** — 12 in `test_acme` (DB-persisted cert
  record, acmex error surfacing, per-CA/per-provider issue paths,
  hot-reload of the served cert), 12 in `test_auth` (login rate limiting,
  bootstrap must-change flag, default password policy), 2 in
  `test_admin_misc` and 1 in `test_templates` (DB write failure → 500),
  3 in `test_backup` (archive perms, auth-token hygiene in dumps), 1 in
  `test_files` (pagination bounds), 1 in `test_security` (malformed E2EE
  IV → 400). The 2026-10-12 audit's "10 additions" counted only the
  then-current tree; note `test_admin_sso_in_catalog` in `test_rbac` is
  **not** Rust-only — it exists in the baseline too, and `test_rbac` maps
  1:1 by name (41/41).

Port the concurrent-saves test together with the G2 fix; run `cargo test` as
part of closing any gap that touches startup, sessions, or DB writes (the
audit itself ran on a static-analysis basis only).

## How to maintain this document

1. When a gap is fixed, flip its Status to `Closed (<date>, <PR/issue>)` and
   note any behavior change in `CHANGELOG.md`.
2. When a new divergence is found (by audit, bug report, or QA), append it
   with the next G-number, a priority tier, where it lives, and its impact.
3. The next full parity re-audit (against a newer Python baseline or after a
   large fix batch) replaces the gap table; keep a dated snapshot of any
   superseded version in the issue thread rather than in this file.
4. Keep this file consistent with [api.md](./api.md) — any route added,
   removed, or changed belongs in both.
