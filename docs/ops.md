# Vreckan (Rust) — Ops & Deployment Guide

Deployment, infrastructure, and operations reference for the Rust port.
Build/deployment specifics follow the ANT-3 multi-stage Dockerfile + compose
and the ANT-6 (in progress) Gitea vLLM-pattern CI/CD work.

## 1. Deployment model

The server is a **single container** (`vreckan-http`) that does everything:
serves the web client, runs the API, terminates TLS, and manages the
per-session application containers through the host's Docker daemon.

```
Browser ──HTTPS:443──▶ vreckan-http (privileged)
                          │  /var/run/docker.sock ──▶ Docker daemon
                          │  /dev/dri, group_add 990/44 (GPU/DRI3)
                          ▼
                    app containers (created on demand, on the host network)
```

Key properties of the default `docker-compose.yml`:

| Setting | Value | Why |
|---|---|---|
| `image` | the published `ghcr.io/vreckan-project/vreckan` tag — `:latest` by default; pin a versioned tag (e.g. `:v0.6.0`) and drop `build:` in a real deployment (Rust cutover complete, ANT-40) | `build: .` is the local fallback: `docker compose build` / `up --build` builds the **Rust** image from this repo and tags it under the `image:` name, so a local checkout runs the Rust server even without a registry |
| `ports` | `443:443` | Single TLS port for API + session proxy + web client |
| `privileged: true` | — | the server creates/manages app containers and needs broad host access (device nodes, cgroups for GPU paths) |
| `ipc: host` | — | shared memory for the streamed apps |
| `devices: /dev/dri:/dev/dri` | — | DRI3 render-node passthrough for GPU-accelerated apps |
| `group_add: 990, 44` | — | host `video` (990) and `input` (44) groups so the server process can open DRI nodes and propagate render-node access to app containers |
| `volumes: ./vreckan-data:/data` | — | all durable state (DB, storage, SSL, caches, backups) in one bind mount |
| `volumes: /var/run/docker.sock:/var/run/docker.sock` | — | Docker Engine API access (the `bollard` client speaks to the daemon over this socket) |
| `env_file: .env` | — | `VRECKAN_*` configuration (see the [README](../README.md) configuration tables and `.env.example`) |

An optional **`postgres:16`** service is included in the compose file for
production Postgres deployments (see §4); with the default SQLite backend it
can be left stopped/removed.

The server binds `0.0.0.0:$VRECKAN_API_PORT` (default **443**) with TLS
(rustls/ring). `VRECKAN_SESSION_PORT` (default 8443) is the internal
session-proxy port; deployments that want session traffic on a separate
published port set it and publish it — the server discovers its published
port mappings from the Docker daemon at startup.

## 2. Quick start

```bash
git clone https://github.com/vreckan-project/vreckan.git
cd vreckan
cp .env.example .env    # edit to taste (OIDC, Let's Encrypt, ...)
docker compose up -d
```

First run: a self-signed certificate is generated under `/data/ssl`, the
database is created, and a bootstrap `admin` account is seeded (default
`admin` / `admin1234`, configurable via `VRECKAN_BOOTSTRAP_ADMIN_*`, printed
to the server logs). Sign in at `https://<your-server>/` and change the
password.

Local build instead of pulling the image:

```bash
docker build -t vreckan .   # multi-stage: rust:1.99-slim build → debian:trixie-slim runtime
```

## 3. Image build, registries, and the CI/CD flow

### 3.1 The image

The `Dockerfile` is multi-stage:

- **Build stage** — `rust:1.99-slim` (Debian trixie): `cargo build --release`
  with BuildKit cargo-cache mounts. System deps are `linux-libc-dev`,
  `pkg-config`, `libssl-dev` (needed by the `-sys` crates and the native-tls
  stack) plus `sccache`, which wraps `rustc` (`RUSTC_WRAPPER=sccache`) and
  persists the compile cache across builds via a BuildKit `type=cache` mount.
- **Test stage** — the build stage plus the `tests/` and `server/` trees: CI
  builds `--target test` and pre-compiles `cargo test --profile test-release`
  (thin-LTO, codegen-units=16) so the Gitea test job only executes the tests.
  sccache serves unchanged dependency crates from its persistent cache, and the
  job count scales with the runner's core count and a per-rustc memory budget.
- **Runtime stage** — `debian:trixie-slim`: the compiled `vreckan` binary,
  the web client (`server/static/`), and the bundled template schema
  (`server/template_schema.yml`). No Python, no pip. TLS, SQLite, and the
  client are all self-contained.

### 3.2 The vLLM-style split (GitHub light, Gitea heavy)

CI follows the vLLM/Gitea pattern: GitHub Actions do the cheap parts, a
self-hosted **Gitea Actions runner** does the heavy build/test/scan work.
GitHub is the source of truth; Gitea is a mirror.

- **`gitea-mirror-trigger.yml`** (the only workflow in this repo; its
  `push`/`pull_request` triggers are re-enabled — they were briefly disabled
  while the initial Rust-port push was landing):
  - *mirror-and-trigger* — force-mirrors the branch to the Gitea mirror repo
    (`scripts/mirror-to-gitea.sh`; PRs mirror to `pr/<number>`) and dispatches
    the Gitea-side **`build-test-rust.yml`** workflow (`scripts/trigger-gitea.sh`)
    with `branch`/`sha` inputs.
  - *bump-and-tag* — on push to `main`: reads `VERSION`; if tag
    `v<VERSION>` does not exist yet, mints a short-lived GitHub App
    installation token, creates and pushes the annotated tag, and mirrors the
    tag (and the commit as `main`) to Gitea. **The Gitea Release workflow
    fires on that tag push** and builds + pushes the container image.
- **Gitea-side workflows** live in the separate `vreckan/vreckan-workflow`
  repo (not this one): `build-test-rust.yml` (build + test + scan on the
  Gitea runner), the **Release (Rust)** workflow (tag → image → registries),
  and the **nightly scan** workflow (§3.4). See ANT-57 for the staged
  enablement of this flow on the Rust port.

### 3.3 Registries

- **GHCR** — `ghcr.io/vreckan-project/vreckan` is the registry; every release
  publishes a versioned tag (`:v<VERSION>`) alongside `:latest`, and the
  compose file's `image:` tag points at it (Rust cutover complete, ANT-40).
- **Gitea registry** — the Release workflow also pushes to the self-hosted
  Gitea package registry for air-gapped/on-prem pulls.
- The compose file's `image:` tag is set by the cutover decision; `build: .`
  always builds the Rust image locally when the tag is not present.

### 3.4 Base-image pinning (`.base-image-digest`)

`.base-image-digest` pins the digest of the **build** base image
(`rust:1.99-slim`). The nightly scan workflow compares the live registry
digest of that base against the `vreckan.base-digest` label of the last
nightly image; when they differ it opens a bot PR re-pointing the file. The
runtime base (`debian:trixie-slim`) is a fixed tag and is not part of the
digest convention. When a digest re-point lands, the release notes should
mention the base-image bump (see [release process](./release.md)).

### 3.5 Self-hosted Gitea runner

The Gitea runner that executes `build-test.yml` / Release needs:

- Gitea with the **Actions** feature enabled; a runner registered with a
  `self-hosted` label.
- A **Docker daemon** reachable from the runner (image build + push).
- Outbound network to GHCR and the Rust/crates registries (the build stage
  fetches the full dependency tree).
- Secrets available to the workflow repo: registry credentials for image
  pushes (GHCR + Gitea registry), plus the `GITEA_URL`/`GITEA_REPO`/
  `GITEA_TOKEN` pair the trigger scripts use.

## 4. Database

All durable configuration (users, groups, roles, apps, templates, sessions,
tokens, shares, settings) lives in one of **13 tables**; tables are created
and missing columns migrated idempotently on every start (SeaORM
schema-sync, mirroring the Python `init_db` + `migrate_db` pair).

Backends are selected with `VRECKAN_DB_BACKEND`:

| Backend | How to configure | Notes |
|---|---|---|
| `sqlite` (bare-binary default) | nothing — file at `VRECKAN_DB_PATH` (default `{data_root}/vreckan.db`) | zero-setup; the default when running the server directly (no compose) |
| `postgres` (compose default) | `VRECKAN_DB_BACKEND=postgres` + `VRECKAN_DB_HOST` / `VRECKAN_DB_PORT` / `VRECKAN_DB_USER` / `VRECKAN_DB_PASS` / `VRECKAN_DB_NAME` — or just point at the bundled compose `postgres` service: `VRECKAN_DB_BACKEND=postgres VRECKAN_DB_HOST=postgres` (the compose service runs `postgres:16` with user/db `vreckan`, password `vreckan`) | recommended for production; the default credentials in the compose file should be changed |
| `mysql` | same pattern (`VRECKAN_DB_BACKEND=mysql`, default port 3306, default user `root`) | MariaDB also accepted |

`VRECKAN_DATABASE_URL` overrides everything with an explicit sqlx-format URL.

**Deployment default.** As of v0.6.0 the `docker-compose.yml` in this repo
points the server at the bundled `postgres` service out of the box: the
`vreckan-http` service sets
`VRECKAN_DATABASE_URL=postgres://vreckan:vreckan@postgres:5432/vreckan` (with
`${VRECKAN_DB_USER:-vreckan}` / `${VRECKAN_DB_PASS:-vreckan}` /
`${VRECKAN_DB_NAME:-vreckan}` defaults) and only starts once that service is
healthy. The bare-binary default remains SQLite, so a deployment that does not
use this compose file is unchanged. To use your own Postgres (or to fall back
to SQLite) set `VRECKAN_DATABASE_URL` in `.env` — the `${VAR:-default}` form
keeps `.env` as the override source while Postgres is the out-of-the-box
default.

### 4.1 Cutting over an existing SQLite deployment

`scripts/migrate-sqlite-to-postgres.py` moves an existing SQLite configuration
database onto a Postgres database, so a deployment that ran on SQLite up to
v0.5.0 can be cut over to the v0.6.0 Postgres default without re-seeding users
and settings. It creates the Postgres schema (idempotently), then replaces each
table's contents with the migrated rows (JSON columns move from SQLite TEXT to
`jsonb`). It is safe to re-run. The server should be stopped during the
cut-over; it needs Python 3.8+ (stdlib `sqlite3`) and `psycopg2`
(`pip install psycopg2-binary`) only for a real run — `--dry-run` previews the
DDL and row counts with the stdlib alone.

```bash
# preview (no Postgres needed)
python3 scripts/migrate-sqlite-to-postgres.py --dry-run

# run it (defaults from VRECKAN_SQLITE_PATH / VRECKAN_DATABASE_URL)
python3 scripts/migrate-sqlite-to-postgres.py
```

**Backups:** the admin UI can create a backup (tar.gz of the data tree + a
logical JSON dump in table order) and restore it, including a chunked
restore flow for large archives; `VRECKAN_BACKUP_RETENTION` prunes old
archives. Take a backup before upgrading the image.

**Full cutover procedure:** the end-to-end runbook for a running deployment —
pre-cutover snapshot, stopping the legacy service, the dry-run and real
migration, row-count and health verification, starting the Rust service
against the bundled Postgres, and the rollback path — is in the
[cutover runbook](./cutover.md). Its two helpers,
`scripts/snapshot-sqlite.py` (a stdlib-only online backup of the SQLite
database) and `scripts/verify-cutover.py` (independent per-table row-count
comparison plus an `/api/health` check), are covered there step by step.

## 5. TLS

- **Self-signed (default).** On first run the server generates
  `/data/ssl/vreckan_dev.crt` / `.key` and reuses them on subsequent starts.
  Good for LAN/VPN deployments; browsers will warn.
- **Let's Encrypt (optional).** Enable via env (`VRECKAN_LE_*`) or the admin
  UI Certificates page (DB value overrides env):
  - `VRECKAN_LE_ENABLED=true`, `VRECKAN_LE_DOMAINS=example.com,www.example.com`
  - Challenge: `VRECKAN_LE_AUTH=dns-cloudflare` (with
    `VRECKAN_LE_CLOUDFLARE_EMAIL` + `VRECKAN_LE_CLOUDFLARE_TOKEN`; works
    behind a reverse proxy) or `http-01` (ACME server fetches over HTTP).
  - `VRECKAN_LE_STAGING=true` for test certificates; renewal runs in the
    background (`VRECKAN_LE_AUTO_RENEW`, check every
    `VRECKAN_LE_RENEW_CHECK_HOURS` hours, renew within
    `VRECKAN_LE_RENEW_BEFORE_DAYS` days of expiry).
  - Issuance/renewal happens through a **transient certbot container**
    launched via the Docker socket, so the server image itself stays
    certbot-free. Until a valid LE certificate exists, the server falls back
    to the self-signed one.

## 6. Host privileges & devices (docker-sock, DRI, groups)

| Privilege | What it does | When you can drop it |
|---|---|---|
| `privileged: true` | broad host access for the server (needed for some container-creation paths and GPU setups) | can be relaxed to specific capabilities if you are certain which app-container features you use; test thoroughly |
| `/var/run/docker.sock` | the server talks to the Docker Engine API to create/inspect/stop app containers and to run the transient certbot container | required unless you run a dedicated provider; a restricted socket proxy (e.g. rootless docker context) is a possible hardening |
| `/dev/dri:/dev/dri` + `group_add: 990` (video) | DRI3 render-node passthrough: GPU-accelerated apps render through the host GPU; the server detects render nodes (and their group ids) at startup and propagates device + supplementary GIDs into app containers | drop if no GPU apps are used |
| `group_add: 44` (input) | host `input` group so Uinput-based input devices work inside app containers | drop if no app needs it |
| `ipc: host` | shared memory for streamed desktop apps | keep unless you have tested the apps without it |

App containers also run with a default identity of `VRECKAN_PUID` /
`VRECKAN_PGID` (both default 1000) for file ownership in the storage tree.

## 7. Data layout (`/data`)

Everything durable lives under the data root (`VRECKAN_DATA_ROOT`, default
`/data` — the `vreckan-data` bind mount):

```
/data
├── vreckan.db                 SQLite database (sqlite backend)
├── storage/
│   ├── <user home dirs>       per-user home directories served to apps as /config
│   ├── vreckan_uploads/       temporary chunked-upload staging
│   ├── vreckan_app_icons/     custom app icons
│   ├── vreckan_home_templates/ meta-app home-dir templates
│   └── vreckan_public/        publicly shared files
├── ssl/                       TLS certs: self-signed (vreckan_dev.*) and Let's Encrypt
├── autostart_cache/           cached autostart scripts
├── app_stores_cache/          cached app-store YAML files
└── backups/                   backup archives
```

## 8. Observability

- `GET /api/health` — liveness probe (`ok (N session(s))`); point load
  balancers/monitors at it.
- Logs: `tracing`-based, structured. `VRECKAN_LOG_LEVEL` (or `RUST_LOG`)
  sets the filter; SQL query logging is available via the SeaORM
  `sqlx_logging_level` hook.
- First-run credentials, cert selection, and startup loads are all logged —
  `docker compose logs -f vreckan-http` is the primary diagnostic surface.
