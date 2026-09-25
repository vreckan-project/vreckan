# Vreckan — History: the hard fork of SealSkin 0.3.2

Vreckan is a **hard fork of SealSkin 0.3.2** (the upstream's `VERSION` file;
`release-notes/` runs 0.3.0 → 0.3.2). This document catalogs **every difference**
between the untouched upstream tree (`~/work/sealskin-original`) and the fork as
it stood on 2026-09-23, and quantifies how much of the original was rewritten.

> **Development since the fork** is tracked in [`CHANGELOG.md`](./CHANGELOG.md).
> The "Post-fork" notes in the tables below are cross-referenced there; see also
> `~/work/memory.md` (§13.x) for full detail.

---

## 1. Headline: how much was rewritten?

Source code was compared file-by-file (paths relative to each project root),
with a path-mapping layer so files moved by the 2026-09-23 reorg and the
re-added i18n dictionaries are diffed at their new location rather than counted
as removed+new. **40.6% of the original's 52,863 source lines were rewritten or
removed.** (Recomputed 2026-09-25 against the current tree.)

| View | % of original rewritten | Basis |
|---|---|---|
| **Headline (path-based)** | **40.6%** | 21,470 of 52,863 original lines changed or have no Vreckan counterpart |
| Excl. i18n translation JSON | **68.1%** | The client's 39,327 lines include 21,481 lines of machine-maintained translation files (18 languages); excluding them, 21,361 of 31,382 lines were rewritten |

**From the Vreckan side:** of its 67,230 source lines, **~47% (31,393 lines)
are carried over** from the original (the 18 i18n dictionaries, the
near-verbatim room port, and same-path files) and **~53% (35,837 lines) are
genuinely new** — the hand-written SPA, the 8 router modules, the test suite,
the security/CSP work, and the i18n key additions layered onto the carried-over
dictionaries.

### Per-area breakdown

| Area | Orig. lines | Rewritten/removed | % of area | New lines in Vreckan |
|---|---:|---:|---:|---:|
| `client/` (esbuild SPA) | 39,327 | 11,382 | 28.9% | 10,957 |
| `server/` (backend + SPA) | 10,799 | 7,725 | 71.5% | 19,379 |
| `docs/` (Next.js site) | 1,284 | 1,284 | 100% | 0 |
| `.github/` (CI workflows) | 754 | 754 | 100% | 0 |
| `mobile/` (Capacitor) | 209 | 209 | 100% | 0 |
| `browser_extension/` | 111 | 111 | 100% | 0 |
| `LICENSE` | 373 | 0 | 0% | 0 |
| `.gitignore` | 6 | 5 | 83% | 24 |

The `client/` row no longer reads 100%: the 18 i18n dictionaries (21,481 lines)
and the collaboration room (6,934 lines) were carried over into
`server/static/`, so only the genuinely re-authored client code (11,382 lines)
counts as rewritten. The rest of the upstream web UI (extension + mobile shells,
options/popup UIs) was deleted and replaced by the hand-written SPA in
`server/static/`. (The `client/` "new lines" column counts the i18n key
additions layered onto the carried-over dictionaries, which the path-mapping
layer attributes to the original `client/` area.)

---

## 2. What was removed

### 2.1 Whole sub-projects deleted

| Upstream component | Size | Fate in Vreckan |
|---|---:|---|
| `client/` — esbuild-built web UI (served pages + extension/mobile shells), incl. **21,481 lines of i18n JSON for 18 languages** | 39,327 lines / 68 files | **Replaced** by the hand-written SPA in `server/static/` (see §4.2). No build step, no bundler. The 18 i18n dictionaries and the room UI were carried over into `server/static/`; the rest (extension + mobile shells, options/popup UIs) was dropped. |
| `browser_extension/` — Chrome/Firefox extension (manifests, build script, icon) | 111 lines | **Deleted.** The web app is the only client. |
| `mobile/` — Capacitor iOS wrapper (Xcode project, build script) | 209 lines | **Deleted.** |
| `docs/` — Next.js documentation website (Fumadocs-style, MDX content, version switcher, search API) | 1,284 lines / 24 files | **Deleted.** No docs site ships with Vreckan. |
| `.github/` — CI, docs, mobile, prerelease, release workflows + dependabot | 754 lines | **Deleted.** No CI in the fork. |
| `release-notes/` (0.3.0–0.3.2) | — | Not carried over. |

### 2.2 Server modules removed or consolidated

| Upstream file | Lines | Fate in Vreckan |
|---|---:|---|
| `app/routers/` — **11 router modules** (`admin` 742, `uploads` 217, `sessions` 203, `launch` 159, `files` 283, `shares` 235, `ui` 102, `internal` 57, `homedirs` 54, `applications` 79, `handshake` 37) | 2,168 | **Consolidated** into the single `app/api.py` (248 → 5,652 lines). All routes now live in one module on `encrypted_router` / `admin_router` / plain routers. (Post-fork 2026-09-20: **split back out** — the routes now live in 8 modules under `app/routers/` again and `api.py` shrank 5,652 → 3,135 lines; see the `server/app/api.py` row and memory.md §13.17.) |
| `app/launch.py` | 919 | Folded into `api.py` (`_launch_common` + 4 launch endpoints). |
| `app/config_store.py` | 937 | **Replaced** by `app/db.py` (328 lines, new) — SQLAlchemy-async tables instead of the old config store. |
| `app/security.py` | 449 | **JWT (RS256) auth removed.** The E2EE half (RSA-OAEP-wrapped AES-256-GCM handshake, `EncryptedRoute`) was re-implemented inside `api.py`; authentication is now **opaque DB-backed tokens** (`auth_tokens` table + `sealskin_auth` cookie). |
| `app/docker_utils.py` | 312 | Folded into `api.py` / `providers/docker_provider.py`. |
| `app/persistence.py` | 207 | **Replaced** by `app/db.py` (sessions, tokens, etc. now persist to SQLite/Postgres). |
| `app/fsutil.py` | 134 | Folded into `api.py`. |
| `app/main.py` + `app/__main__.py` + `server/main.py` | 146 | **Caddy removed.** Upstream rendered `Caddyfile.tpl` and ran Caddy as a child process in front of uvicorn; Vreckan's `run_https.py` (156 lines, new) serves TLS directly and reverse-proxies session traffic **in-process**. |
| `app/Caddyfile.tpl` | 71 | **Deleted** (no external proxy anymore). |
| `app/state.py` | 92 | Folded into `api.py` / `db.py` (in-memory maps + crypto sessions). |
| `app/version.py` | 53 | **Deleted.** |
| `app/__init__.py`, `providers/__init__.py`, `routers/__init__.py` | 3 | Dropped (namespace packages). |
| `server/tests/` — 6 test files (`conftest`, `test_api_smoke`, `test_config_store`, `test_launch`, `test_persistence`, `test_security`) | 789 | **Replaced** by a new 6-file suite (1,103 lines + `pytest.ini`): `conftest`, `test_auth`, `test_api_smoke`, `test_launch`, `test_db`, `test_security`. `test_config_store` and `test_persistence` have no counterpart — the config store and YAML persistence they covered no longer exist (see `app/db.py`). Hermetic: per-test temp sandboxes, wiped DB, fresh asyncio locks, network/docker no-ops; the real lifespan runs per test. Run with `docker exec vreckan-http python -m pytest tests/`. (Post-fork 2026-09-18: two more files added — `test_oidc.py` (440 lines, 16 tests; mock IdP on a local `ThreadingHTTPServer`) and `test_backup.py` (345 lines, 17 tests; sandboxed data root) — the suite is now 8 files / 1,870 lines / **79 tests**, 0 warnings. See memory.md §13.13. Post-fork 2026-09-19: four Tier 2 files added — `test_people.py` (408 lines, 30 tests), `test_volume_mounts.py` (242 lines, 16 tests), `test_stores.py` (479 lines, 29 tests), `test_templates.py` (328 lines, 21 tests) — the suite is now 12 files / 3,327 lines / **175 tests**, 0 warnings. Tier 2 uncovered 10 real bugs in `api.py` (shadowed `except` clauses swallowing explicit 400/422/500 responses, unguarded body parsing, a double-wrapped 500); all fixed. See memory.md §13.14. Post-fork 2026-09-19 (Tier 3): four more files added — `test_admin_misc.py` (271 lines, 15 tests), `test_user_sessions.py` (160, 13), `test_user_homedirs.py` (81, 8), `test_pinned.py` (134, 11) — plus 2 more tests in `test_auth.py` (change_password 422; live session survives a password change) and a strengthened wrong-old-password assertion — the suite is now 15 files / 3,994 lines / **224 tests**, 0 warnings. Tier 3 found no app bugs (the newly covered endpoints were clean). See memory.md §13.15. Post-fork 2026-09-19 (Tier 4): two more files added — `test_upload.py` (222 lines, 16 tests) and `test_files.py` (648 lines, 49 tests) — the suite is now 17 files / 4,864 lines / **289 tests**, 0 warnings. Tier 4 uncovered 5 real bugs in `api.py` (a path-traversal write-primitive via the client-controlled `upload_id`, 2× unguarded request-body parsing, a shadowed `except` in create_public_share, a negative `chunk_index` 500, and a create_folder-on-file 500); all fixed. See memory.md §13.16. Post-fork 2026-09-20 (Tier 5): `test_docker_launch.py` (181 lines, 2 tests) added — the first tier to drive the *real* `DockerProvider` against the local Docker daemon (launch → ready → stop lifecycle over the compose network; a no-GPU app gets no `/dev` passthrough); both tests **skip (not fail) when no Docker daemon is reachable**, so the suite still runs on Docker-less hosts — the suite is now 18 files / 5,045 lines / **291 tests**, 0 warnings. See memory.md §13.17. Post-fork 2026-09-23: `test_rbac.py` (575 lines) added for the RBAC rewrite — the suite is now 19 files / **342 tests**. Post-fork 2026-09-24/25: `test_security.py` grew with security-header, strict-CSP, and session-proxy-CSP tests — the suite is now 20 files / 5,948 lines / **345 tests**.) |
| `server/pyproject.toml` + `server/setup.py` | 69 | **Replaced** by `requirements.txt` + `Dockerfile` (container-first deployment). (Post-fork 2026-09-18: `requirements.txt` pins `httpx2` — not `httpx` — as the app's HTTP client; `httpx` was also uninstalled from the live container. See memory.md §13.12. Post-fork 2026-09-24/25: `sqlalchemy` → `sqlalchemy[asyncio]` — the `[asyncio]` extra pulls in `greenlet`, which the dev container was missing and which crash-looped the server on every `docker compose --force-recreate`.) |

---

## 3. What changed in place (files present in both)

41 files have a counterpart in both trees (14 at the same path, 27 moved by the
reorg or the room/i18n port). Line counts and the fraction of original lines
that changed (diff-based), highest change first:

| File (orig → vreckan) | Orig → New | % of original changed | What changed |
|---|---|---:|---|
| `server/app/static/index.html` → `server/static/index.html` | 262 → 328 | 96% | Rebuilt as the SPA shell (sidebar nav, views, modals incl. the launch modal); 2026-09-23: instrumented with `data-i18n` attributes + the sidebar language selector. |
| `server/tests/test_api_smoke.py` → `tests/test_api_smoke.py` | 212 → 109 | 84% | Re-scoped to the reorganized API surface. |
| `.gitignore` (root) | 6 → 25 | 83% | Rewritten in explicit-ignore style (2026-09-23). |
| `server/app/api.py` → `server/api.py` | 248 → 3,295 | 82% | Absorbed all 11 routers + `launch.py` + `docker_utils` + `fsutil` + `state`; added OIDC/SSO, opaque-token auth, E2EE routes, session persistence, app stores, templates, GPU, backup/restore, volume mounts, session naming. (Post-fork 2026-09-18: 11× `.dict()`→`.model_dump()` to clear Pydantic deprecation warnings; HTTP client migrated `httpx`→`httpx2` — 17 refs; see memory.md §13.12. Post-fork 2026-09-19: 10 bug fixes uncovered by the Tier 2 test suite — 7× shadowed `except Exception` clauses swallowing explicit 400/422/500 responses, 3× unguarded request-body parsing, a double-wrapped 500 in app-store YAML processing — plus 1× `.copy(deep=True)`→`.model_copy()`; see memory.md §13.14. Post-fork 2026-09-19 (Tier 4): 5 more bug fixes uncovered by the Tier 4 test suite — a path-traversal write-primitive via the client-controlled `upload_id` (new `_validated_upload_path` realpath-containment helper used by upload_chunk + _reassemble_file → 404), 2× unguarded request-body parsing (create_folder, initiate_deletion → 422), a shadowed `except Exception` in create_public_share (now re-raises HTTPException so 400/403/404 propagate), a negative `chunk_index` → 422 (Query ge=0), and create_folder-on-a-file → 400; see memory.md §13.16. Post-fork 2026-09-20: **split into router modules** — all routes moved out into 8 modules under `app/routers/` (`session`, `homedir`, `pinned`, `admin`, `upload`, `files`, `auth`, `encrypted` — 2,879 lines); `api.py` is now **3,135 lines** and keeps only the module-level state, shared helpers, handshake endpoints, and router wiring. Routers reference shared state via `api.X` at call time so the hermetic test fixtures keep working. See memory.md §13.17.) |
| `server/tests/test_launch.py` → `tests/test_launch.py` | 201 → 353 | 78% | Expanded for session naming, presets, GPU, room mode. |
| `server/app/routers/admin.py` → `server/routers/admin.py` | 742 → 1,690 | 77% | All admin endpoints (people, groups, roles, stores, templates, sessions, GPU, backup, volume mounts). |
| `server/tests/test_security.py` → `tests/test_security.py` | 30 → 231 | 77% | E2EE handshake + opaque-token auth coverage. (Post-fork 2026-09-24/25: security-header, strict-CSP, and session-proxy-CSP tests added.) |
| `server/app/user_manager.py` → `server/user_manager.py` | 483 → 1,212 | 71% | OIDC/SSO provisioning, `is_sso` flag, groups, admin/user unification, SSO-aware password rules. |
| `server/app/routers/files.py` → `server/routers/files.py` | 283 → 430 | 69% | File browser, shares, uploads. |
| `server/app/providers/base_provider.py` → `server/providers/base_provider.py` | 55 → 60 | 62% | Interface tweaks for the new launch kwargs. |
| `server/app/models.py` → `server/models.py` | 597 → 567 | 58% | New request/response models (launch `session_name`, `ActiveSessionInfo.name`, stores, templates, presets); JWT models dropped. |
| `server/app/providers/docker_provider.py` → `server/providers/docker_provider.py` | 384 → 393 | 57% | GPU passthrough (DRI3 device + forwarded supplementary groups), env-layering for templates, session relaunch support. (Post-fork 2026-09-18: HTTP client migrated `httpx`→`httpx2` — 5 refs; see memory.md §13.12.) |
| `server/tests/conftest.py` → `tests/conftest.py` | 110 → 344 | 53% | Hermetic per-test sandboxes, wiped DB, fresh asyncio locks, network/docker no-ops; the real lifespan runs per test. |
| `server/app/settings.py` → `server/settings.py` | 290 → 346 | 42% | `VRECKAN_*` env prefix, OIDC settings, 443 ports, LE cert paths. (Post-fork: 12 vestigial definitions deleted, 3 missing ones added — `database_url`, `data_root`, `backups_path` — cookie names rebranded to `vreckan_*`, path defaults repointed onto `/data`; see memory.md §13.11. Post-fork 2026-09-18: a 4th missing definition added — `backup_retention` (int, default 0 = keep all); `backup_manager._apply_retention()` referenced it but it was never defined, so **every `POST /backup/create` 500'd in production** (zero behavior change for existing deployments). See memory.md §13.13.) |
| `server/app/logging_config.py` → `server/logging_config.py` | 24 → 23 | 42% | Minor. |
| `server/app/collaboration.py` → `server/collaboration.py` | 950 → 909 | 20% | Room serving adapted to the in-process proxy + new room assets. (Post-fork 2026-09-18: HTTP client migrated `httpx`→`httpx2` — 2 refs; see memory.md §13.12. Post-fork 2026-09-24/25: the injected `COLLAB_DATA` script now carries a CSP nonce and the room response sets a strict, nonce-based CSP.) |
| `server/app/static/public_password.html` → `server/static/public_password.html` | 248 → 232 | 18% | Restyle. |
| `client/src/ui/room/room.css` → `server/static/collaboration/room.css` | 1,664 → 1,603 | 10% | **Ported near-verbatim** (room UI). |
| `client/src/ui/room/room.js` → `server/static/collaboration/room.js` | 3,614 → 3,507 | 5% | **Ported near-verbatim** (room UI). |
| `client/src/ui/room/room.html` → `server/static/collaboration/room.html` | 146 → 161 | 2% | **Ported near-verbatim** (room UI). |
| `client/src/ui/room/translation.js` → `server/static/collaboration/translation.js` | 1,510 → 1,512 | 1% | **Ported near-verbatim** (room UI; the room keeps its own self-contained i18n). |
| `client/src/i18n/*.json` (18 files) → `server/static/i18n/*.json` | 21,481 → 32,119 | 0.5% | **Re-added 2026-09-23** — the 18-language dictionaries carried over; the new Vreckan-specific keys (login/nav/launch/files/password/share/common) were added, translated into all 18 languages. (Post-fork 2026-09-24/25: more keys added — the short "Control"/"Stores" nav labels, the Advanced-settings warning + backup link, and the removed login tagline — translated into all 18 languages; the dictionaries grew to 32,119 lines.) |
| `server/app/template_schema.yml` → `server/template_schema.yml` | 900 → 1,210 | 0.3% | 311 lines **added** (new settings); the original 138-setting schema is ~99.9% carried over verbatim. |
| `LICENSE` | 373 → 373 | 0% | Untouched. |

---

## 4. What's new in Vreckan

### 4.1 New backend modules

| File | Lines | Purpose |
|---|---:|---|
| `server/app/db.py` | 342 | SQLAlchemy-async data layer: 12 tables (`users`, `groups`, `volume_mounts`, `app_stores`, `installed_apps`, `app_templates`, `template_schema_settings`, `pinned_behaviors`, `app_settings`, `public_shares`, `sessions`, `auth_tokens`), startup migrations, DB↔memory sync, `db_dump`/`db_restore`. Replaces `config_store.py` + `persistence.py` + `state.py`. (Post-fork fix: `db_restore` now decodes the raw JSON-column strings from the dump before re-inserting — the original restore double-encoded every JSON column, corrupting backups on restore; covered by `tests/test_db.py`.) |
| `app/backup_manager.py` | 328 | Full backup/restore (tarball, chunked transfer over the E2EE channel). No upstream equivalent. (Post-fork 2026-09-18: its `backup_retention` knob was missing from `settings.py` → every `POST /backup/create` 500'd; fixed by adding the setting, and the module is now covered by `tests/test_backup.py` (17 tests). See memory.md §13.13.) |
| `app/volume_mount_manager.py` | 98 | Admin-configurable external host volumes for app containers. |
| `run_https.py` | 151 | New entrypoint: uvicorn with TLS (Let's Encrypt cert when mounted, self-signed fallback) — replaces the Caddy+uvicorn supervisor. (Post-fork: dead keys/groups/auth_tokens env pins removed — those stores are DB-backed now.) |

### 4.2 New frontend — hand-written SPA (no framework, no build step)

| File | Lines | Notes |
|---|---:|---|
| `static/index.html` | 328 | SPA shell: sidebar nav, Apps/Files/Overview + 9 admin views, modals; instrumented with `data-i18n` attributes + the sidebar language selector (2026-09-23). (Post-fork 2026-09-24: the sidebar + login brand now use the Vreckan wordmark; the login tagline was removed.) |
| `static/css/app.css` | 1,881 | Complete site theme (upstream styling lived in `client/src/ui/css/`). (Post-fork 2026-09-24/25: layout utility classes added so the admin panels' inline `style=""` attributes could be dropped for a strict CSP.) |
| `static/js/app.js` (+ `js/app/` modules) | 507 + 928 | Core SPA: nav, app grid, launch modal (incl. optional **session naming**), active-session list; i18n wiring (language selector, `vreckan:langchange` re-render). The app-area logic is split across `js/app/` (homedirs, launch, nav, pinned, sessions). |
| `static/js/admin.js` (+ `js/admin/` modules) | 272 + 3,307 | All admin views (people, groups, apps, stores, templates, sessions, GPU, backup). The admin-area logic is split across `js/admin/` (accounts, apps, backup, global-settings, groups, laboratory, mounts, overview, roles, sessions, sso, stores, templates). (Post-fork 2026-09-24/25: inline `style=""` attributes in these panels replaced with utility classes for the strict CSP; BETA/ADVANCED tags, the Advanced-settings warning + backup link, and the group-form dropdown removal landed here.) |
| `static/js/files.js` | 662 | Files view + "Open in app" flow. |
| `static/js/i18n.js` | 170 | **New (2026-09-23)** — i18n runtime: loads the 18-language dictionaries, deep-merges each over English, `t()` with ICU-style plurals + placeholders, `applyTranslations()`, `setLanguage()`. |
| `static/js/modal.js` | 182 | Confirm/alert modal system. |
| `static/js/crypto.js` | 132 | E2EE client (handshake + AES-GCM) for `secureFetch`. |
| `static/js/api.js` | 63 | `secureFetch` wrapper. |
| `static/js/auth.js` | 60 | Login/logout/SSO wiring. |
| `static/i18n/` (18 language JSON files) | 32,119 | **Re-added 2026-09-23** from the upstream client's i18n dir; the new Vreckan-specific keys were added and translated into all 18 languages. (Post-fork 2026-09-24/25: the short "Control"/"Stores" nav labels, the Advanced-settings warning + backup link, and the removed login tagline were added — the dictionaries grew from 22,716 to 32,119 lines.) |
| `static/collaboration/` (room.js 3,507 + translation.js 1,512 + room.css 1,603 + room.html 161) | 4,783 | **Ported from the upstream client's room UI** — 94.8% line-level carry-over (6,574 of 6,934 original lines match; only 360 lines changed). |
| `static/img/` (favicon, icon + mark PNGs, Vreckan wordmark + icon SVGs) | — | New assets. (Post-fork 2026-09-24: the full logo kit added — `vreckan-wordmark.svg` (transparent + black), `vreckan-icon-transparent.svg`, and 16/32/64/256/512px PNGs — used by the sidebar and login page.) |

**Dropped from the upstream client:** the esbuild build pipeline (`build.mjs`,
462 lines), the browser-extension and mobile shells, and the options/popup UIs.
The **i18n dictionaries were re-added** on 2026-09-23 (see above); the room keeps
its own self-contained `translation.js`.

### 4.3 New deployment files

| File | Lines | Purpose |
|---|---:|---|
| `Dockerfile` | 44 | Production image (`python:3.12-slim`). Upstream shipped no Dockerfile. (Post-fork: a RUN layer sed-patches starlette's test-only `testclient.py` to clear an anyio `BlockingPortal` alias deprecation warning — required because starlette 1.6.0 + anyio 4.15.1 are both at latest. See memory.md §13.12.) |
| `docker-compose.yml` | 61 | Dev deployment (GPU passthrough, LE cert mount, 443). |
| `docker-compose.prod.yml` | 117 | Production deployment template (8443, named volume, healthcheck). |
| `docker-compose.test.yml` | 55 | Isolated test variant (8444). |
| `dev-container.sh` | 50 | Dev container up/stop/remove wrapper. |
| `.dockerignore`, `.env.example` | 40 | Image hygiene + documented env vars. |

---

> **Note (2026-09-23 reorg, see CHANGELOG.md):** the deploy files above now
> live at the repository root, and the app package moved from `server/app/` to
> `server/` (imports are `server.*`, the uvicorn target is
> `server.api:api_app`). The paths in this section describe the original fork
> layout.

---

## 5. Feature-level changes

### 5.1 Replaced

| Upstream (0.3.2) | Vreckan |
|---|---|
| JWT (RS256) bearer auth, client-signed tokens | **Opaque, DB-backed tokens** (`auth_tokens` table, 8 h TTL, `sealskin_auth` cookie — the cookie name is a legacy holdover from the rename) |
| Caddy reverse proxy as a child process | **In-process** FastAPI reverse proxy for session traffic |
| esbuild SPA (`client/`) with 18-language i18n | **Hand-written SPA** in `server/static/`, no build step; i18n **re-added 2026-09-23** (18-language dictionaries + a small runtime + a sidebar language selector) |
| `config_store.py` + `persistence.py` + `state.py` | Single **SQLAlchemy-async `db.py`** (SQLite default, Postgres optional) |
| 11-module `routers/` package | **Single `api.py`** (5,652 lines) — since split back into 8 router modules (Post-fork 2026-09-20; see §2.2/§3) |
| `main.py` (Caddy + uvicorn supervisor) | `run_https.py` (TLS uvicorn, LE or self-signed) |

### 5.2 Kept (carried over)

- **End-to-end encryption** for all `/api/*` except auth/handshake (RSA-OAEP
  handshake → AES-256-GCM session key; `EncryptedRoute` + client `secureFetch`).
- **Selkies** WebRTC streaming for app sessions.
- The **Docker provider** model for launching/managing app containers.
- The **collaboration room** (ported near-verbatim, §4.2).
- The **138-setting template schema** (YAML seed, ~99.9% unchanged).
- `LICENSE`, and the general FastAPI + Pydantic architecture.

### 5.3 Added (no upstream equivalent)

- **OIDC/SSO** (authentik) with group→admin mapping and the `is_sso` user flag.
- **Session persistence** — sessions (incl. container registry + launch kwargs)
  survive API restarts/reboots; upstream sessions were memory-only.
- **Backup/restore** of the whole data volume (chunked over E2EE).
- **Admin volume mounts** (external host volumes into app containers).
- **App stores + Installed Apps** (118-app store cache, install/uninstall, search filter).
- **App templates** with the schema **default/current split** (push-through only
  when the admin changed a value from the image baseline).
- **Per-user default presets** and a **global default GPU** (admin).
- **GPU passthrough** (Intel DRI3 device + forwarded supplementary groups).
- **Optional session naming at launch** (`TITLE="<App> - <Name>"` env override;
  creation-time only).
- **Admin/user unification** (single people API; an admin is a user with
  `is_admin=true`).
- **In-page alert modals** in the collaboration room (replacing native `alert()`).
- **Set-password** control for local users (SSO users excluded).
- **Let's Encrypt / 443** deployment with hostname-derived OIDC redirect URIs.
- **Security response headers + CSP** (Post-fork 2026-09-24/25): a middleware
  in `api.py` sets CSP, HSTS, `X-Content-Type-Options`, `X-Frame-Options`,
  Permissions-Policy, COEP, COOP, and CORP on every response. The default CSP
  is strict (no `'unsafe-inline'`); the collaboration room and public password
  page use a per-response nonce, and the reverse-proxied session page keeps a
  looser CSP for third-party app HTML. A ZAP full-scan is clean
  (0 High / 0 Medium / 0 Low). See the 2026-09-24/25 changelog entry.

---

## 6. Rewrite metric — methodology

- **Baseline:** every hand-written *source* file in `sealskin-original`
  (`.py .js .mjs .ts .tsx .html .css .yml .yaml .toml .cfg .ini .sh .ps1 .json
  .tpl` + `Dockerfile`/`Caddyfile`/`.dockerignore`/`.gitignore`/`.env.example`/
  `LICENSE`), keyed by path relative to the project root. Excluded: markdown
  docs, binary assets, lock files, `.env` (secrets), runtime data.
- **Original: 154 files / 52,863 lines.** Vreckan: **105 files / 67,230
  lines.** 41 files have a counterpart in both trees (14 at the same path, 27
  moved by the reorg or the room/i18n port — diffed at their new location via a
  path-mapping layer), 113 original files have no Vreckan counterpart (counted
  100% rewritten), 64 Vreckan files are new. (Recomputed 2026-09-25; the
  Vreckan side grew since the 2026-09-23 snapshot — test suite, admin SPA,
  security work, i18n key additions.)
- **% rewritten** = (original lines that were changed in a common file, or
  belong to a removed file) ÷ (all original source lines) = 21,470 / 52,863 =
  **40.6%**.
- **Adjustment** (see §1): excluding the 21,481 i18n JSON lines → **68.1%**
  (21,361 of 31,382). This is the more meaningful figure: the i18n dictionaries
  were carried over, not re-authored.
- **Caveat:** the router consolidation means some upstream `routers/` code
  survives inside `api.py`; the path-based metric counts those router lines as
  "removed" even where the code was merely relocated, so the headline slightly
  overstates true re-authoring.
- Reproduce with: `python3 ~/work/_rewrite_analysis2.py` (the reorg-aware
  variant; the original `~/work/_rewrite_analysis.py` predates the reorg and
  the i18n re-add).
- **Layout caveat:** the metric is computed against the post-reorg layout
  (app code at `server/`, deploy files at the repo root) with a path-mapping
  layer that also maps the re-added i18n dictionaries
  (`client/src/i18n/` → `server/static/i18n/`) and the room port
  (`client/src/ui/room/` → `server/static/collaboration/`).

---

