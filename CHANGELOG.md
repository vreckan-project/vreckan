# Vreckan — Changelog

Vreckan is a hard fork of [SealSkin 0.3.2](https://github.com/linuxserver/docker-sealskin).
The full fork analysis — what was removed, rewritten, and added relative to the
upstream tree — lives in [`HISTORY.md`](./HISTORY.md). This changelog tracks
development *after* the fork, newest first. The Python-era entries below date
from the squashed git history of the original fork; the Rust-port era (2026-10)
is committed normally in `vreckan-rust`. The changelog is the authoritative
record of what shipped in each release — see
[`docs/release.md`](./docs/release.md) for the conventions.

## 2026-10-13 — Python-to-Rust switchover, v0.6.0 (ANT-40)

- **v0.6.0 release.** `VERSION` and `Cargo.toml` bumped to 0.6.0; the
  switchover cuts the deployment over from the Python/FastAPI server to the
  Rust (axum/SeaORM) server.
- **Postgres is now the default deployment database.** `docker-compose.yml`
  bundles a `postgres:16` service and points the server at it by default via
  `VRECKAN_DATABASE_URL`; the service is health-gated so the server only
  starts once it is ready. Override in `.env` to use your own Postgres or to
  fall back to SQLite. See [docs/ops.md §4](./docs/ops.md).
- **SQLite → Postgres migration tool.** New `scripts/migrate-sqlite-to-postgres.py`
  moves an existing SQLite configuration database onto a Postgres database so
  a running deployment can be cut over without re-seeding users and settings.
  Idempotent (safe to re-run); `--dry-run` previews the DDL and row counts
  without touching the target.
- **Repo switchover.** The Rust port becomes the primary code in `vreckan`;
  the legacy Python server moves to the `vreckan-python` archive repo (its
  GitHub Actions workflows are disabled there).

## 2026-10-12 — Rust port kickoff (ANT-1)

- **Initial Rust port of the server** — the Python/FastAPI/uvicorn server is
  rewritten as a single static Rust binary (`vreckan`; axum + tokio, SeaORM on
  SQLite/PostgreSQL/MySQL, `ring` for the E2EE crypto, `bollard` for the
  Docker provider, `tokio-tungstenite` for the WebSocket legs). All 103 unique
  endpoint paths of the original are present (one additive `GET /api/health`),
  the web client under `server/static/` is carried over byte-identical, and
  the test suite is ported 1:1 by name (387 of 388 original tests; 10
  Rust-only additions).
- **Parity audit (ANT-5)** — full static-analysis parity audit against the
  Python original (Gitea baseline `e7b2de6`, v0.5.0): route tables
  machine-extracted and diffed, handler behavior verified. Result: complete
  route coverage, 11 documented gaps (G1–G11, 3× P1) handed to core dev. The
  gap list is maintained in [`docs/port-status.md`](./docs/port-status.md).
- **Multi-stage Docker image + compose (ANT-3)** — `rust:1.99-slim` build
  stage, `debian:trixie-slim` runtime (no Python in the image);
  `docker-compose.yml` for the Rust image (TLS 443, `vreckan-data` bind
  mount, Docker socket, `/dev/dri` + group add, optional Postgres service);
  `.base-image-digest` pins the build base image for the nightly-scan digest
  flow.
- **Documentation (ANT-8)** — README updated for the Rust port; new
  [`docs/`](./docs/) tree: `api.md` (endpoint reference with auth/E2EE
  requirements), `ops.md` (deployment, registries, Gitea runner/CI, databases,
  TLS, data layout), `release.md` (release & changelog process), and
  `port-status.md` (known-gaps living record).

## 2026-09-24 / 2026-09-25 — Branding, UI polish, DAST hardening
- **Vreckan wordmark** in the sidebar and on the login page (replaces the
  icon + name); the login tagline is gone; the "or" divider between the
  password and SSO buttons is styled. The full logo kit (wordmark, icon,
  PNG sizes) now ships in `static/img/`.
- **Admin UI polish** — Installed Apps shows the installed-apps table above
  the install-from-store card; the sidebar reads "Control" / "Stores";
  Templates and Laboratory carry red BETA tags (sidebar + card titles); the
  Advanced settings card gets an amber ADVANCED tag, a "changes can break
  app launches" warning, and a link to the Backup page; the group add/edit
  forms drop the vestigial group dropdown. All new/changed strings
  translated into all 18 languages.
- **Security headers on every response** (middleware in `api.py`): CSP,
  HSTS, `X-Content-Type-Options`, `X-Frame-Options: SAMEORIGIN`,
  Permissions-Policy, COEP, COOP, CORP.
- **Strict CSP (Phase 2)** — the default CSP carries no `'unsafe-inline'`:
  the main app's 95 inline `style=""` attributes became utility classes in
  `app.css`; the collaboration room and the public password page use a
  per-response nonce for their inline script/style; the reverse-proxied
  session page (third-party noVNC/app HTML) keeps a looser CSP with
  `'unsafe-inline'`.
- **`requirements.txt`**: `sqlalchemy` → `sqlalchemy[asyncio]` (pulls in
  greenlet, which the dev container was missing and which crash-looped the
  server on recreate).
- **ZAP full-scan clean: 0 High / 0 Medium / 0 Low** (8 informational noise
  items remain). Test suite now **345 tests / 20 files**.

## 2026-09-23 — RBAC, SSO config page, bootstrap admin, directory reorg
- **RBAC rewrite** — a 17-permission catalog in `server/permissions.py`
  (12 admin + 5 user, plus the `admin` super-permission); effective
  permissions are the union of direct grants, roles, and groups; `is_admin`
  is derived from the `admin` permission. Per-endpoint permission gating in
  the routers.
- **SSO group re-stamp gating** — on each SSO login, only groups present in
  the configured admin/user group lists get their roles re-stamped (prevents
  IdP groups from leaking roles). Group cards on the Groups page are now
  editable in place.
- **SSO config page** — SSO settings moved to a DB-over-env model: the
  `app_settings` table wins over the environment, with per-field source
  badges (db/env/default). New admin **SSO page** (connection + group
  mapping, masked client secret, tag inputs, live connection test) behind a
  new `admin.sso` permission (18th in the catalog). Endpoints:
  `GET/PUT /api/admin/sso`, `POST /api/admin/sso/test`. All `VRECKAN_OIDC_*`
  env vars are now optional (commented out in `.env`; the app runs purely
  from the DB).
- **Configurable bootstrap admin** — `VRECKAN_BOOTSTRAP_ADMIN_USERNAME` /
  `VRECKAN_BOOTSTRAP_ADMIN_PASSWORD` (defaults `admin` / `admin1234`), read
  at seed time; the delete/demote/role-strip protection guards resolve the
  bootstrap username dynamically (DB first, then env).
- **Directory reorganization** — everything under `server/` moved up one
  level: the app package `server/app/` is now `server/` (imports renamed
  `app.*` → `server.*`; uvicorn target `server.api:api_app`), and the deploy
  files (`Dockerfile`, `docker-compose*.yml`, `run_https.py`,
  `requirements.txt`, `pytest.ini`, `.env`, `.env.example`, `.dockerignore`,
  `dev-container.sh`) plus `tests/` now live at the repository root. The
  `vreckan-data` bind mount is now `./vreckan-data`.
- **`.gitignore` rewritten in explicit-ignore style** (junk listed; everything
  else tracked by default) — replacing the old `*/` whitelist in
  `server/.gitignore`, which had silently left `app/routers/`,
  `app/static/{js,css,img}/`, and `tests/` untracked.
- **i18n re-added to the SPA** — the 18-language dictionaries from the
  upstream client were restored to `server/static/i18n/` (the fork had dropped
  them when the esbuild client was replaced). A small runtime
  (`server/static/js/i18n.js`) loads the dictionaries, deep-merges each
  language over English (the non-English files are partial), and exposes
  `t()` with ICU-style plurals + placeholders, `applyTranslations()`, and
  `setLanguage()`. The static DOM is instrumented with `data-i18n`
  attributes, and a **language selector** in the sidebar footer switches the
  UI live (persisted to `localStorage`, honored on the next load; the room
  page keeps its own self-contained `translation.js`). The new
  Vreckan-specific keys (login/nav/launch/files/password/share/common) were
  translated into all 18 languages.
- **Test suite now 342 tests / 20 files**, all green in the dev container.

## 2026-09-21 / 2026-09-22 — ecdsa (Minerva) resolution
- **Verified the ecdsa Minerva timing-attack finding unreachable** at runtime
  (jose uses the cryptography backend for EC whenever cryptography is
  installed; the live IdP signs RS256/RSA).
- **Removed `ecdsa` from the image** (`pip uninstall` in the Dockerfile + dev
  container) to keep security scanners quiet.

## 2026-09-20 — Docker lifecycle tests, router split, base-image update
- **Tier 5 test coverage** — real Docker lifecycle tests.
- **`api.py` split** — the 5,652-line monolith was split back into 8 router
  modules under `app/routers/` (`admin`, `auth`, `encrypted`, `files`,
  `homedir`, `pinned`, `session`, `upload`); `api.py` shrank to ~3,135 lines.
- **Package-update pass** — dev base image advanced to debian 13.7; security
  re-scan clean.

## 2026-09-19 — Test coverage Tiers 2–4
- **Tier 2** — people, volume mounts. **Tier 3** — admin misc, user sessions,
  user homedirs. **Tier 4** — uploads + files.

## 2026-09-18 — Test hygiene + HTTP client
- **Test-warning cleanup** (all 29 pytest warnings resolved) and the **httpx →
  httpx2 migration** (the app's HTTP client; `httpx` also uninstalled from the
  live container).
- **Tier 1 test coverage** — OIDC + backup/restore.

## 2026-09-16 — Templates, launch UX, first test suite
- **Template settings separated from global settings** (per-app overrides).
- **Template schema default/current split** — push-through only when the admin
  changed a value from the image baseline.
- **Launch modal file-toggle** for file-backed apps; **optional session naming
  at launch** (`TITLE="<App> - <Name>"` env override, creation-time only).
- **First test suite** — `tests/` (6 files, 46 tests) + `pytest.ini`.
- **Settings audit + SealSkin→Vreckan rebrand** — 12 vestigial settings
  deleted; naming swept across the codebase.

## 2026-09-15 — Deployment + auth groundwork
- **Migrated to `0.0.0.0:443` + Let's Encrypt + the real hostname**
  (`vreckan.antitux.net`); the OIDC redirect URI is derived per-request from
  the Host header, so SSO works on any access path.
- **Admin/user unification** — a single people API; an admin is a user with
  `is_admin=true`. Added the `users.is_sso` column (SSO users have no local
  web password); the two SSO test accounts were flagged manually.
