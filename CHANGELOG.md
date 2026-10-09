# Vreckan — Changelog

Vreckan is a hard fork of [SealSkin 0.3.2](https://github.com/linuxserver/docker-sealskin).
The full fork analysis — what was removed, rewritten, and added relative to the
upstream tree — lives in [`HISTORY.md`](./HISTORY.md). This changelog tracks
development *after* the fork, newest first. Full detail lives in
`~/work/memory.md` (§13.x); the git history was squashed into a single
"initial commit" (2026-09-23), so this log is the authoritative record.

Since **v0.4.0** the project ships tagged container releases (published to the
Gitea registry and GHCR). The release entries below summarize what each tag
contains; the date-based entries that follow give the finer-grained
development history.

## 2026-10-09 — v0.4.6 — Security hardening
A focused pass over the codebase and its dependencies.
- **OIDC: `python-jose` → `PyJWT`.** `python-jose` 3.5.0 is affected by
  CVE-2026-85394 (Critical, algorithm-confusion guard bypass) with no patched
  release yet. The app now verifies `id_token`s with PyJWT, **pins the
  signature algorithm to RS256** (instead of trusting the token header), and
  **fails closed** when the provider's JWKS is unreachable. A new
  `VRECKAN_OIDC_REQUIRE_SIGNATURE` setting (default `true`) restores the
  legacy claims-only fallback for providers that omit a JWKS.
- **E2EE: AES-GCM now binds the session id as AAD** on both the server and the
  web client, so a captured ciphertext can't be replayed against a different
  session's key. `CRYPTO_SESSIONS` also stores a last-seen timestamp and a
  background sweeper reaps sessions idle for more than 24 h, fixing the
  unbounded in-memory growth.
- **GPU detection no longer shells out.** `detect_gpus()` reads the sysfs
  `device/driver` symlinks directly instead of a `shell=True` `ls`/`awk`
  pipeline.
- **Public-share passwords use argon2id** (matching account passwords) instead
  of bare SHA-256. Existing SHA-256 shares keep working — the verifier selects
  the comparison from the stored hash's format, so no migration is needed.
- **Deferred (documented, not changed):** the admin-triggered app-store fetch
  SSRF surface, self-hosting Font Awesome (served behind Cloudflare), and
  making `SELKIES_ALLOWED_ORIGINS` configurable (a previous attempt broke the
  Selkies iframes).

## 2026-10-09 — v0.4.4 — Deployment consolidation
- **One compose file.** The three compose variants (dev / prod / test) were
  consolidated into a single `docker-compose.yml`: image-based
  (`ghcr.io/vreckan-project/vreckan:latest`), TLS on 443, a persistent
  `vreckan-data` volume, Docker-socket + `/dev/dri` passthrough, and a bundled
  `postgres:16` service. Removed `docker-compose.prod.yml`,
  `docker-compose.postgres.yml`, `docker-compose.test.yml`, and
  `dev-container.sh`.
- **Docs pass.** README, the project site (index / setup / FAQ), HISTORY, and
  PRIVACY were updated to match the current single-endpoint architecture, the
  consolidated compose file, and the real bootstrap-admin and port defaults.

## 2026-10-09 — v0.4.3 — Base-image bump (nightly)
- The nightly scan detected a newer `python:3.12-slim` base and opened a PR;
  merged and tagged. The image re-patches its OS packages via `apt dist-upgrade`
  at build time, so the bump keeps the Trivy scan clean.

## 2026-10-08 — v0.4.2 — Dockerfile simplification
- Rewrote the `Dockerfile` (74 → 26 lines): the starlette `BlockingPortal`
  patch and the `ecdsa` uninstall now live in a single `RUN` layer, and the
  entrypoint is just `python run_https.py`. Dropped the verbose per-layer
  comments.

## 2026-10-08 — v0.4.1 — CI/CD consolidation onto Gitea
- **Retired the GitHub build workflows** (`ci.yml`, `release.yml`,
  `image-build.yml`, `auto-merge-bump.yml`); the build / test / scan / release
  pipeline now runs on the Gitea runner (see the `vreckan-workflow` repo).
- **Merged the auto-merge-bump job into `gitea-mirror-trigger.yml`** — one run,
  two jobs: mirror GitHub → Gitea, then trigger the build.
- **App-token attribution.** The bump-and-tag step mints a short-lived
  `vreckan-ci` GitHub App token (JWT → installation token) instead of using a
  personal `GH_TOKEN`, so version bumps and tags are attributed to the app bot.
- **Hardened the Gitea dispatch.** The `GITEA_INPUTS` JSON is now built
  in-script from scalar env vars (a stray `}` in inline YAML had broken the
  dispatch), and the auth URL is constructed by scheme-split rather than fragile
  string substitution.

## 2026-10-08 — v0.4.0 — First container release
The first tagged release. It bundles the work that landed since the fork
(detailed in the 2026-09-25 and 2026-10-01 entries below):
- **Let's Encrypt / certbot** — `server/le_certbot.py` issues and renews certs
  via a transient certbot container on a shared `vreckan-le` volume (Cloudflare
  DNS-01 or HTTP-01); self-signed by default; a new admin **Certificates** page
  and `admin.certificates` permission; configurable auto-renew.
- **Project site** — the Jekyll site for vreckanproject.com (overview +
  setup / usage / FAQ), styled on the app's design tokens.
- **Bulk app updates** — check-all / update-all endpoints and a session
  "older image" flag, in the admin and user UIs.
- **PostgreSQL validation** — the test suite is now dialect-aware (runs
  identically against SQLite or PostgreSQL); new `test_postgres.py` covers the
  JSONB / migration / dump-restore paths.
- **Gitea mirror + container build/push** — the `gitea-mirror-trigger.yml`
  workflow mirrors GitHub → Gitea and triggers the build; the Gitea runner
  builds, tests, scans (Trivy), and pushes images to the Gitea registry and
  GHCR with versioned + timestamped + `latest` tags.
- **Base-image hardening** — `apt dist-upgrade` at build time keeps the OS
  packages patched (Trivy-clean).

## 2026-10-01 — Let's Encrypt, project site, bulk app updates
- **Let's Encrypt / certbot** — new `server/le_certbot.py`: issue and renew
  certs via a transient certbot/dns-cloudflare container on a shared
  `vreckan-le` volume; Cloudflare DNS-01 and HTTP-01; cert status/expiry
  helpers. Self-signed by default; LE enabled via env (`VRECKAN_LE_*`) or the
  new admin **Certificates** page (UI edits override env, like SSO).
  Configurable auto-renew (check every N hours, renew within N days of expiry);
  background renewal job in `api.py`; `run_https.py` picks the LE cert at
  startup when enabled, else falls back to self-signed. New
  `admin.certificates` permission, Certificates page (`js/admin/certificates.js`),
  sidebar entry, i18n, and GET/PUT/renew endpoints. `vreckan-le` volume added to
  the compose files; `VRECKAN_LE_*` block in `.env.example`.
- **Project site (`site/`)** — Jekyll site for vreckanproject.com: overview +
  setup / usage / FAQ docs, styled on the app's design tokens and using the
  project logos. The setup guide is Docker-image-first (pull
  `ghcr.io/vreckan-project/vreckan`).
- **Bulk app updates** — `models.py`: `AppUpdateCheckResult` /
  `CheckAllUpdatesResponse` / `AppImagePullResult` / `PullAllImagesResponse`;
  session "older image" flag. Bulk check-all / update-all endpoints and session
  recreate in the admin + user UIs; tests in `test_stores.py`.
- **Tests** — new `test_le_certbot.py` (29 tests); full suite green on SQLite
  and Postgres.

## 2026-09-25 — PostgreSQL validation; dialect-aware test suite
- The configuration database is selected by `VRECKAN_DATABASE_URL` and the
  schema/queries are backend-agnostic, but the test suite was hardwired to
  SQLite. It now runs identically against either backend so PostgreSQL is
  actually exercised: `asyncpg` added to requirements; `tests/conftest.py`
  honors an externally-set `VRECKAN_DATABASE_URL`, drives the per-test DB wipe
  through the app's engine, and adds a `sqlite3`-compatible `db_conn` shim
  (placeholder translation, `information_schema` for `sqlite_master`, JSONB →
  JSON-text normalization, upsert); the seven direct-DB test files point at the
  shim; new `test_postgres.py` covers the dialect-specific paths (JSONB column
  types, legacy-schema migration ALTERs, JSONB round-trips, db_dump/db_restore,
  session persistence).

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
