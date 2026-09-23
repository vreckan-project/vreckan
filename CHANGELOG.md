# Vreckan — Changelog vs. SealSkin 0.3.2

Vreckan is a **hard fork of SealSkin 0.3.2** (the upstream's `VERSION` file;
`release-notes/` runs 0.3.0 → 0.3.2). This document catalogs **every difference**
between the untouched upstream tree (`~/work/sealskin-original`) and this project
(`~/work/vreckan`), and quantifies how much of the original was rewritten.

---

## 1. Headline: how much was rewritten?

Source code was compared file-by-file (paths relative to each project root).
**94.1% of the original's 52,863 source lines were rewritten or removed.**

| View | % of original rewritten | Basis |
|---|---|---|
| **Headline (path-based)** | **94.1%** | 49,731 of 52,863 original lines changed or have no Vreckan counterpart |
| Excl. i18n translation JSON | **90.0%** | The client's 39,327 lines include 21,481 lines of machine-maintained translation files (18 languages) that were dropped, not "rewritten" |
| Also crediting the room port | **69.1%** | The collaboration room (6,934 original lines) was **ported near-verbatim** (94.8% line-level carry-over — only 360 lines genuinely changed) but lives at a new path, so the path-based metric counts it as removed+new |

**From the Vreckan side:** of its 25,402 source lines, **~38% (9,706 lines) are
carried over** from the original (3,132 lines in same-path files + 6,574 lines of
the room port) and **~62% (15,696 lines) are genuinely new code**.

### Per-area breakdown

| Area | Orig. lines | Rewritten/removed | % of area | New lines in Vreckan |
|---|---:|---:|---:|---:|
| `client/` (esbuild SPA) | 39,327 | 39,327 | 100% | 0 |
| `server/` (backend + SPA) | 10,799 | 8,041 | 74.5% | 22,266 |
| `docs/` (Next.js site) | 1,284 | 1,284 | 100% | 0 |
| `.github/` (CI workflows) | 754 | 754 | 100% | 0 |
| `mobile/` (Capacitor) | 209 | 209 | 100% | 0 |
| `browser_extension/` | 111 | 111 | 100% | 0 |
| `LICENSE` | 373 | 0 | 0% | 0 |
| `.gitignore` | 6 | 5 | 83% | 4 |

The `client/` row dominates: the entire upstream web UI (plus its extension and
mobile shells) was deleted and replaced by the hand-written SPA that now lives in
`server/app/static/`.

---

## 2. What was removed

### 2.1 Whole sub-projects deleted

| Upstream component | Size | Fate in Vreckan |
|---|---:|---|
| `client/` — esbuild-built web UI (served pages + extension/mobile shells), incl. **21,481 lines of i18n JSON for 18 languages** | 39,327 lines / 68 files | **Replaced** by the hand-written SPA in `server/app/static/` (see §4.2). No build step, no bundler, no i18n (except the ported room). |
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
| `server/tests/` — 6 test files (`conftest`, `test_api_smoke`, `test_config_store`, `test_launch`, `test_persistence`, `test_security`) | 789 | **Replaced** by a new 6-file suite (1,103 lines + `pytest.ini`): `conftest`, `test_auth`, `test_api_smoke`, `test_launch`, `test_db`, `test_security`. `test_config_store` and `test_persistence` have no counterpart — the config store and YAML persistence they covered no longer exist (see `app/db.py`). Hermetic: per-test temp sandboxes, wiped DB, fresh asyncio locks, network/docker no-ops; the real lifespan runs per test. Run with `docker exec vreckan-http python -m pytest tests/`. (Post-fork 2026-09-18: two more files added — `test_oidc.py` (440 lines, 16 tests; mock IdP on a local `ThreadingHTTPServer`) and `test_backup.py` (345 lines, 17 tests; sandboxed data root) — the suite is now 8 files / 1,870 lines / **79 tests**, 0 warnings. See memory.md §13.13. Post-fork 2026-09-19: four Tier 2 files added — `test_people.py` (408 lines, 30 tests), `test_volume_mounts.py` (242 lines, 16 tests), `test_stores.py` (479 lines, 29 tests), `test_templates.py` (328 lines, 21 tests) — the suite is now 12 files / 3,327 lines / **175 tests**, 0 warnings. Tier 2 uncovered 10 real bugs in `api.py` (shadowed `except` clauses swallowing explicit 400/422/500 responses, unguarded body parsing, a double-wrapped 500); all fixed. See memory.md §13.14. Post-fork 2026-09-19 (Tier 3): four more files added — `test_admin_misc.py` (271 lines, 15 tests), `test_user_sessions.py` (160, 13), `test_user_homedirs.py` (81, 8), `test_pinned.py` (134, 11) — plus 2 more tests in `test_auth.py` (change_password 422; live session survives a password change) and a strengthened wrong-old-password assertion — the suite is now 15 files / 3,994 lines / **224 tests**, 0 warnings. Tier 3 found no app bugs (the newly covered endpoints were clean). See memory.md §13.15. Post-fork 2026-09-19 (Tier 4): two more files added — `test_upload.py` (222 lines, 16 tests) and `test_files.py` (648 lines, 49 tests) — the suite is now 17 files / 4,864 lines / **289 tests**, 0 warnings. Tier 4 uncovered 5 real bugs in `api.py` (a path-traversal write-primitive via the client-controlled `upload_id`, 2× unguarded request-body parsing, a shadowed `except` in create_public_share, a negative `chunk_index` 500, and a create_folder-on-file 500); all fixed. See memory.md §13.16. Post-fork 2026-09-20 (Tier 5): `test_docker_launch.py` (181 lines, 2 tests) added — the first tier to drive the *real* `DockerProvider` against the local Docker daemon (launch → ready → stop lifecycle over the compose network; a no-GPU app gets no `/dev` passthrough); both tests **skip (not fail) when no Docker daemon is reachable**, so the suite still runs on Docker-less hosts — the suite is now 18 files / 5,045 lines / **291 tests**, 0 warnings. See memory.md §13.17.) |
| `server/pyproject.toml` + `server/setup.py` | 69 | **Replaced** by `requirements.txt` + `Dockerfile` (container-first deployment). (Post-fork 2026-09-18: `requirements.txt` pins `httpx2` — not `httpx` — as the app's HTTP client; `httpx` was also uninstalled from the live container. See memory.md §13.12.) |

---

## 3. What changed in place (files present in both)

14 files exist at the same relative path in both trees. Line counts and the
fraction of original lines that changed (diff-based):

| File | Orig → New | % of original changed | What changed |
|---|---|---:|---|
| `server/app/api.py` | 248 → 5,652 | 78% | Absorbed all 11 routers + `launch.py` + `docker_utils` + `fsutil` + `state`; added OIDC/SSO, opaque-token auth, E2EE routes, session persistence, app stores, templates, GPU, backup/restore, volume mounts, session naming. (Post-fork 2026-09-18: 11× `.dict()`→`.model_dump()` to clear Pydantic deprecation warnings; HTTP client migrated `httpx`→`httpx2` — 17 refs; see memory.md §13.12. Post-fork 2026-09-19: 10 bug fixes uncovered by the Tier 2 test suite — 7× shadowed `except Exception` clauses swallowing explicit 400/422/500 responses, 3× unguarded request-body parsing, a double-wrapped 500 in app-store YAML processing — plus 1× `.copy(deep=True)`→`.model_copy()`; see memory.md §13.14. Post-fork 2026-09-19 (Tier 4): 5 more bug fixes uncovered by the Tier 4 test suite — a path-traversal write-primitive via the client-controlled `upload_id` (new `_validated_upload_path` realpath-containment helper used by upload_chunk + _reassemble_file → 404), 2× unguarded request-body parsing (create_folder, initiate_deletion → 422), a shadowed `except Exception` in create_public_share (now re-raises HTTPException so 400/403/404 propagate), a negative `chunk_index` → 422 (Query ge=0), and create_folder-on-a-file → 400; see memory.md §13.16. Post-fork 2026-09-20: **split into router modules** — all routes moved out into 8 modules under `app/routers/` (`session`, `homedir`, `pinned`, `admin`, `upload`, `files`, `auth`, `encrypted` — 2,879 lines); `api.py` is now **3,135 lines** and keeps only the module-level state, shared helpers, handshake endpoints, and router wiring. Routers reference shared state via `api.X` at call time so the hermetic test fixtures keep working. See memory.md §13.17.) |
| `server/app/user_manager.py` | 483 → 587 | 74% | OIDC/SSO provisioning, `is_sso` flag, groups, admin/user unification, SSO-aware password rules. |
| `server/app/models.py` | 597 → 493 | 56% | New request/response models (launch `session_name`, `ActiveSessionInfo.name`, stores, templates, presets); JWT models dropped. |
| `server/app/providers/base_provider.py` | 55 → 60 | 62% | Interface tweaks for the new launch kwargs. |
| `server/app/providers/docker_provider.py` | 384 → 393 | 56% | GPU passthrough (DRI3 device + forwarded supplementary groups), env-layering for templates, session relaunch support. (Post-fork 2026-09-18: HTTP client migrated `httpx`→`httpx2` — 5 refs; see memory.md §13.12.) |
| `server/app/settings.py` | 290 → 257 | 21% | `VRECKAN_*` env prefix, OIDC settings, 443 ports, LE cert paths. (Post-fork: 12 vestigial definitions deleted, 3 missing ones added — `database_url`, `data_root`, `backups_path` — cookie names rebranded to `vreckan_*`, path defaults repointed onto `/data`; see memory.md §13.11. Post-fork 2026-09-18: a 4th missing definition added — `backup_retention` (int, default 0 = keep all); `backup_manager._apply_retention()` referenced it but it was never defined, so **every `POST /backup/create` 500'd in production** (zero behavior change for existing deployments). See memory.md §13.13.) |
| `server/app/collaboration.py` | 950 → 901 | 20% | Room serving adapted to the in-process proxy + new room assets. (Post-fork 2026-09-18: HTTP client migrated `httpx`→`httpx2` — 2 refs; see memory.md §13.12.) |
| `server/app/logging_config.py` | 24 → 23 | 42% | Minor. |
| `server/app/static/index.html` | 262 → 314 | 96% | Rebuilt as the SPA shell (sidebar nav, views, modals incl. the launch modal). |
| `server/app/static/public_password.html` | 248 → 232 | 18% | Restyle. |
| `server/app/template_schema.yml` | 900 → 1,210 | 0.1% | 311 lines **added** (new settings); the original 138-setting schema is ~99.9% carried over verbatim. |
| `server/.gitignore` | 9 → 7 | 22% | |
| `.gitignore` (root) | 6 → 5 | 83% | |
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
| `static/index.html` | 314 | SPA shell: sidebar nav, Apps/Files/Overview + 9 admin views, modals. |
| `static/css/app.css` | 1,572 | Complete site theme (upstream styling lived in `client/src/ui/css/`). |
| `static/js/app.js` | 1,269 | Core SPA: nav, app grid, launch modal (incl. optional **session naming**), active-session list. |
| `static/js/admin.js` | 2,801 | All admin views (people, groups, apps, stores, templates, sessions, GPU, backup). |
| `static/js/files.js` | 708 | Files view + "Open in app" flow. |
| `static/js/modal.js` | 182 | Confirm/alert modal system. |
| `static/js/crypto.js` | 132 | E2EE client (handshake + AES-GCM) for `secureFetch`. |
| `static/js/api.js` | 63 | `secureFetch` wrapper. |
| `static/js/auth.js` | 60 | Login/logout/SSO wiring. |
| `static/collaboration/` (room.js 3,507 + translation.js 1,512 + room.css 1,603 + room.html 161) | 4,783 | **Ported from the upstream client's room UI** — 94.8% line-level carry-over (6,574 of 6,934 original lines match; only 360 lines changed). |
| `static/img/` (favicon.svg, favicon-32.png, icon128.png) | — | New assets. |

**Dropped from the upstream client:** the esbuild build pipeline (`build.mjs`,
462 lines), the browser-extension and mobile shells, the options/popup UIs, and
**all i18n** (21,481 lines of JSON across 18 languages — the room keeps its own
`translation.js`).

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

## 5. Feature-level changes

### 5.1 Replaced

| Upstream (0.3.2) | Vreckan |
|---|---|
| JWT (RS256) bearer auth, client-signed tokens | **Opaque, DB-backed tokens** (`auth_tokens` table, 8 h TTL, `sealskin_auth` cookie — the cookie name is a legacy holdover from the rename) |
| Caddy reverse proxy as a child process | **In-process** FastAPI reverse proxy for session traffic |
| esbuild SPA (`client/`) with 18-language i18n | **Hand-written SPA** in `server/app/static/`, no build step, no i18n |
| `config_store.py` + `persistence.py` + `state.py` | Single **SQLAlchemy-async `db.py`** (SQLite default, Postgres optional) |
| 11-module `routers/` package | **Single `api.py`** (5,652 lines) |
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

---

## 6. Rewrite metric — methodology

- **Baseline:** every hand-written *source* file in `sealskin-original`
  (`.py .js .mjs .ts .tsx .html .css .yml .yaml .toml .cfg .ini .sh .ps1 .json
  .tpl` + `Dockerfile`/`Caddyfile`/`.dockerignore`/`.gitignore`/`.env.example`/
  `LICENSE`), keyed by path relative to the project root. Excluded: markdown
  docs, binary assets, lock files, `.env` (secrets), runtime data.
- **Original: 154 files / 52,863 lines.** Vreckan: **37 files / 25,402 lines.**
  14 files are common (diffed with `difflib`), 140 original files have no
  Vreckan counterpart (counted 100% rewritten), 23 Vreckan files are new.
- **% rewritten** = (original lines that were changed in a common file, or
  belong to a removed file) ÷ (all original source lines) = 49,731 / 52,863 =
  **94.1%**.
- **Adjustments** (see §1): excluding the 21,481 i18n JSON lines → **90.0%**;
  additionally crediting the 6,574 room-port lines that were *moved*, not
  rewritten → **69.1%**.
- **Caveat:** the router consolidation means some upstream `routers/` code
  survives inside `api.py`; the path-based metric counts those router lines as
  "removed" even where the code was merely relocated, so the headline slightly
  overstates true re-authoring. The 69.1% figure is the closest to "lines a
  human actually had to rewrite."
- Reproduce with: `python3 ~/work/_rewrite_analysis.py` (report:
  `~/work/_rewrite_report.txt`).
