# Vreckan (Rust) — Ops & Deployment Guide

Deployment, infrastructure, and operations reference for the Rust port.
Build/deployment specifics follow the ANT-3 multi-stage Dockerfile + compose
and the ANT-6 Gitea vLLM-pattern CI/CD work.

## 1. Deployment model

The server is a **single container** (`vreckan-http`) that does everything:
serves the web client, runs the API, terminates TLS, and manages the
per-session application containers through the host's Docker daemon.

```
Browser ──HTTPS:443──▶ vreckan-http (UID 1000, non-root)
                           │  /var/run/docker.sock ──▶ Docker daemon
                           │  /dev/dri, group_add 990/44/docker (GPU/DRI3)
                           ▼
                     app containers (created on demand, on the host network)
```

Since v0.6.1 the `vreckan-http` container runs its server process as a
dedicated non-root user (`vreckan`, UID/GID 1000 — see §6) instead of root
with `privileged: true` (ANT-81, P1-1).

Key properties of the default `docker-compose.yml`:

| Setting | Value | Why |
|---|---|---|
| `image` | the published `ghcr.io/vreckan-project/vreckan` tag — `:latest` by default; pin a versioned tag (e.g. `:v0.6.0`) and drop `build:` in a real deployment (Rust cutover complete, ANT-40) | `build: .` is the local fallback: `docker compose build` / `up --build` builds the **Rust** image from this repo and tags it under the `image:` name, so a local checkout runs the Rust server even without a registry |
| `ports` | `443:443` | Single TLS port for API + session proxy + web client |
| non-root `USER` in the image | UID/GID 1000 | the server process no longer runs as root; the only capability it needs is `cap_net_bind_service` (a file capability on the binary, so the default port 443 binds on any host) because all host-side work (app-container creation, GPU setup) is done by the Docker daemon over the socket (§6) |
| `ipc: host` | — | shared memory for the streamed apps |
| `devices: /dev/dri:/dev/dri` | — | DRI3 render-node passthrough for GPU-accelerated apps |
| `group_add: 990, 44, ${VRECKAN_DOCKER_GROUP:-docker}` | — | host `video` (990) and `input` (44) groups so the server process can open DRI nodes and propagate render-node access to app containers; the `docker` group gives the non-root process access to the host socket (override the name/GID with `VRECKAN_DOCKER_GROUP`) |
| `volumes: ./vreckan-data:/data` | — | all durable state (DB, storage, SSL, caches, backups) in one bind mount |
| `volumes: /var/run/docker.sock:/var/run/docker.sock` | — | Docker Engine API access (the `bollard` client speaks to the daemon over this socket) |
| `data-perms` service + `depends_on` | — | one-shot `chown -R 1000:1000` of `./vreckan-data` before first boot, so the non-root server can write to a bind mount that Docker created root-owned (§7) |
| `env_file: .env` | — | `VRECKAN_*` configuration (see the [README](../README.md) configuration tables and `.env.example`) |

The **`postgres:16`** service is included in the compose file and is the
v0.6.0 default database (see §4); since v0.6.1 its host port is published
on loopback only (`127.0.0.1:5432`) instead of every interface (ANT-90,
P2-6).

The server binds `0.0.0.0:$VRECKAN_API_PORT` (default **443**) with TLS
(rustls/ring) and serves the API, web client, and session proxy on that
single listener. `VRECKAN_SESSION_PORT` (default 8443) is carried-over
metadata from the Python port, which ran the session proxy on a separate
listener; it does not change the bind port in the Rust port.

## 2. Quick start

```bash
git clone https://github.com/vreckan-project/vreckan.git
cd vreckan
cp .env.example .env    # edit to taste (OIDC, Let's Encrypt, ...)
docker compose up -d
```

First run: a self-signed certificate is generated under `/data/ssl`, the
database is created, and a bootstrap `admin` account is seeded (default
`admin` / `admin1234`, configurable via `VRECKAN_BOOTSTRAP_ADMIN_*`). The
account is marked *must-change-password*: the web client prompts for a new
password on first login, and a login with the literal default `admin1234` is
logged at `warn` unless `VRECKAN_DEFAULT_PASSWORD_POLICY` says otherwise
(`allow` / `warn` / `reject`). The startup log names the username but prints
the password value only when the well-known default is in use. Sign in at
`https://<your-server>/` and change the password.

The one-shot `data-perms` service chowns `./vreckan-data` to UID/GID 1000 on
every `docker compose up` before the server starts (see §7), so no manual
ownership step is needed — on a fresh host and when upgrading from a
v0.5.x deployment (whose data tree was written by the old root-running
image), the first `up` after pulling the new image takes ownership over.

Brute-force protection on `/api/auth/login`: per (client IP, username) pair,
`VRECKAN_LOGIN_MAX_ATTEMPTS` (default 5) failed attempts within
`VRECKAN_LOGIN_LOCKOUT_SECONDS` (default 900 s) lock the pair out for the
same window (HTTP 429, fail closed); lockout events are logged at `warn`
and a successful login clears the pair.

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

**Host exposure (v0.6.1, ANT-90 P2-6).** The bundled service is no longer
published on every host interface: its `ports` entry defaults to
`${VRECKAN_POSTGRES_PORT:-127.0.0.1:5432}:5432`, i.e. host loopback only. The
server reaches the database over the internal compose network either way. Set
`VRECKAN_POSTGRES_PORT=5432` in `.env` to publish on every interface again, or
comment out the `ports` entry to disable host publication entirely. The
default `vreckan`/`vreckan` credentials still apply unless overridden
(`VRECKAN_DB_USER`/`VRECKAN_DB_PASS`); since v0.6.1 the server reacts to
starting against those defaults (ANT-45, P2-6). By default it logs a prominent
warning; the `VRECKAN_ALLOW_DEFAULT_DB_CREDS` escape hatch selects the
behaviour — unset/empty warns, `1`/`true` silences the warning once the
defaults are knowingly accepted, and `refuse`/`strict`/`fail` stops the server
from starting. The guard only fires when the *resolved* database URL actually
carries the default `vreckan`/`vreckan` credentials.

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

The target DSN is printed with its `user:pass` userinfo masked
(`postgres://***:***@host:5432/db`), and a connection failure is re-raised
with the DSN scrubbed, so the credentials do not leak into logs or
tracebacks.

**Backups:** the admin UI can create a backup (tar.gz of the data tree + a
logical JSON dump in table order) and restore it, including a chunked
restore flow for large archives. Archives (and the in-archive
`db_dump.json`) are written `0600`, because the dump carries
`users.password_hash`; live session tokens (`auth_tokens`) are excluded
from the dump, so a restore never drops or resurrects in-flight logins.
`VRECKAN_BACKUP_RETENTION` prunes old archives (default: keep 7). Take a
backup before upgrading the image. For a native `pg_dump`-based Postgres
backup with dated-dump rotation, see §4.2.

### 4.2 Postgres backup and restore (`scripts/backup-postgres.sh`)

The admin-UI archives of §4.1 snapshot the data tree (plus a logical JSON
dump of the database). For a server-independent, cron-friendly database
backup, `scripts/backup-postgres.sh` writes a native PostgreSQL
custom-format dump (`pg_dump -Fc`) of the vreckan database as a dated file
(`vreckan-postgres-YYYYmmdd-HHMMSS.dump`) and keeps only the newest
`VRECKAN_BACKUP_RETENTION` of them (0 = keep all; the script's default is
14).

**Configuration.** The script reads the same database configuration as the
server (§4): `VRECKAN_DATABASE_URL` (postgres:// URL, wins over the
discrete vars), or `VRECKAN_DB_BACKEND` / `VRECKAN_DB_HOST` /
`VRECKAN_DB_PORT` / `VRECKAN_DB_USER` / `VRECKAN_DB_PASS` /
`VRECKAN_DB_NAME`. Run it with the same environment the server uses — for
a compose deployment, source the same `.env` first:

```bash
set -a; . ./.env; set +a
```

Execution picks the lightest available path:

- **Bundled compose Postgres** — when the `vreckan-postgres` container
  (`VRECKAN_POSTGRES_CONTAINER` to rename) is present and the target host
  is local, `pg_dump`/`pg_restore` run inside it via `docker exec`. No
  Postgres client is needed on the host, and the container's own
  `POSTGRES_USER`/`POSTGRES_PASSWORD`/`POSTGRES_DB` initialization values
  are used when the operator did not override the URL.
- **External Postgres / no container** — the host's `pg_dump` /
  `pg_restore` are used (install `postgresql-client`), connecting to the
  configured host/port with the configured credentials.

**Where dumps go.** `VRECKAN_BACKUP_DIR` (default
`${VRECKAN_DATA_ROOT:-.}/backups`). For a compose deployment set it to the
*host* path of the `vreckan-data` mount (e.g.
`VRECKAN_BACKUP_DIR=./vreckan-data/backups`) so the dated dumps live next
to the admin-UI archives and a snapshot of the `vreckan-data` volume
covers both.

**Usage:**

```bash
scripts/backup-postgres.sh backup      # write a dated dump, rotate to the
                                        # retention count
scripts/backup-postgres.sh list        # stored dumps, newest first
scripts/backup-postgres.sh restore vreckan-postgres-YYYYmmdd-HHMMSS.dump
#   FILE is a path, or a name inside VRECKAN_BACKUP_DIR. Stop the vreckan
#   server first (it re-syncs the schema on startup). The default
#   --clean --if-exists drops and recreates the dumped objects;
#   --no-clean restores over the existing schema.
```

**Scheduling.** Cron (daily at 03:17):

```cron
17 3 * * * cd /opt/vreckan && set -a && . ./.env && set +a && ./scripts/backup-postgres.sh backup >> /var/log/vreckan-backup.log 2>&1
```

or a systemd timer:

```ini
# /etc/systemd/system/vreckan-backup.service
[Unit]
Description=Vreckan Postgres backup
[Service]
Type=oneshot
WorkingDirectory=/opt/vreckan
EnvironmentFile=/opt/vreckan/.env
ExecStart=/opt/vreckan/scripts/backup-postgres.sh backup
```

```ini
# /etc/systemd/system/vreckan-backup.timer
[Unit]
Description=Daily Vreckan Postgres backup
[Timer]
OnCalendar=daily
Persistent=true
[Install]
WantedBy=timers.target
```

(`systemctl enable --now vreckan-backup.timer`.)

**Restoring elsewhere.** Because the dump is a standard `pg_dump -Fc`
archive it restores to *any* Postgres 11+ cluster: point
`VRECKAN_DATABASE_URL` at the new cluster and run
`scripts/backup-postgres.sh restore FILE` (or use `pg_restore` directly).
For full disaster recovery, restore into a fresh (dropped/recreated)
database and then start the server — the startup schema-sync reconciles
the rest.

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

Since v0.6.1 the `vreckan-http` container runs its server process as a
dedicated non-root user (`vreckan`, UID/GID 1000) and no longer uses
`privileged: true` (ANT-81, P1-1). The split:

- **What the server process itself needs** — bind
  `0.0.0.0:$VRECKAN_API_PORT` (default **443**, a privileged port below
  1024), read `/sys` and `/proc`, write under `/data` and `/config`
  (image-owned or chowned, see §7), and talk to the host Docker daemon over
  the socket. The only capability involved is `cap_net_bind_service`,
  granted as a **file capability on the `vreckan` binary** in the image
  (like nginx's unprivileged image), so the bind works on any host —
  including ones that set `net.ipv4.ip_unprivileged_port_start=1024`
  (RHEL/SUSE) — without a compose-level `cap_add`. Everything else needs no
  capabilities.
- **What the daemon does for it** — creating/inspecting/stopping app
  containers and the transient certbot container, plus the host-side parts
  of GPU/cgroup setup, all happen in the (root) Docker daemon, so they no
  longer need this container to be privileged. If a specific app-container
  feature turns out to require a capability on the *server container*, add
  it as a targeted `cap_add` entry in `docker-compose.yml` (test thoroughly)
  instead of restoring `privileged: true`.

**Socket strategy (v0.6.1, ANT-81 P1-1).** The default deployment keeps the
**raw `/var/run/docker.sock`** — the same choice the SealSkin reference
deployment makes — with the container's access scoped to the host `docker`
group instead of `privileged: true`. The blast radius of the raw socket is
that of the daemon's API surface: a compromised *server process* can
create/inspect/stop containers and read container logs, which on a standard
host is equivalent to root (a container created with a host root bind mount
escapes). It is *no longer* an automatic root upgrade via `privileged`, and
it is the minimum surface needed for the feature set: the server creates
app containers **with device mounts and `group_add`** (that is how a
DRI3/GPU app gets its render node — a scoped `docker-socket-proxy` must
allow those container-creation fields or GPU app launches silently break),
runs the transient certbot container, and discovers its own published port
mappings. If you want to restrict the API surface, run
`vreckan-http` against a scoped socket proxy (e.g. `docker-socket-proxy`
with `POST /containers/create`, `GET/POST /containers/...`, `POST
/containers/.../start|stop|kill` and network/image reads allowed) or a
rootless Docker context — both are drop-in because the server only speaks
the standard Engine API over the mounted socket path.

| Privilege | What it does | When you can drop it |
|---|---|---|
| `group_add: 990` (video) | DRI3 render-node passthrough: GPU-accelerated apps render through the host GPU; the server detects render nodes (and their group ids) at startup and propagates device + supplementary GIDs into app containers | drop if no GPU apps are used |
| `group_add: 44` (input) | host `input` group so Uinput-based input devices work inside app containers | drop if no app needs it |
| `group_add: ${VRECKAN_DOCKER_GROUP:-docker}` | the non-root server process reaches the host Docker socket (owned `root:docker`, mode 660) through the host's docker group — the replacement for `privileged: true`'s blanket access | set `VRECKAN_DOCKER_GROUP` to the group name or GID when the daemon's group is not named `docker`; a scoped socket proxy (e.g. `docker-socket-proxy`) or a rootless docker context is the further-hardening option if you want to restrict the API surface |
| `/var/run/docker.sock` | the server talks to the Docker Engine API to create/inspect/stop app containers and to run the transient certbot container — raw socket by default (see the strategy paragraph above) | required unless you run a dedicated provider; the server degrades gracefully at boot if the socket is missing or inaccessible (logs a warning, app launches and self-inspection are disabled) |
| `/dev/dri:/dev/dri` | render-node device passthrough for GPU-accelerated apps (paired with the `video` group above) | drop if no GPU apps are used |
| `ipc: host` | shared memory for streamed desktop apps | keep unless you have tested the apps without it |

App containers also run with a default identity of `VRECKAN_PUID` /
`VRECKAN_PGID` (both default 1000) for file ownership in the storage tree —
the same UID/GID as the server's non-root user, so the server and the apps
it spawns share ownership of the `/data/storage` tree.

## 7. Data layout (`/data`)

Everything durable lives under the data root (`VRECKAN_DATA_ROOT`, default
`/data` — the `vreckan-data` bind mount).

**Ownership (v0.6.1, ANT-81 P1-1).** The server runs as UID/GID 1000, so
everything it writes under `/data` (and the host path behind the bind
mount) is owned by 1000:1000. Docker creates a missing `./vreckan-data`
directory as root:root, so the compose file includes the one-shot
`data-perms` service (`chown -R 1000:1000`) which `vreckan-http` waits for;
it runs on every `docker compose up` and is a cheap no-op once the tree is
already owned by 1000. Upgrading from a v0.5.x deployment (data written by
the old root-running image) is handled by the first `up` on the new image.
If you point `VRECKAN_DATA_ROOT` elsewhere, apply the same ownership there.

**Volume layout (v0.6.1, ANT-81).** This deployment persists state in a
single `/data` bind mount (`./vreckan-data`). That is a deliberate
deviation from the SealSkin reference, which bind-mounts *both* `/config`
and `/storage` and keeps the E2EE server key at `/config/ssl/`. Here
`/config` is an **ephemeral, image-owned** directory inside the server
container: the first-boot E2EE server key
(`/config/ssl/server_key.pem`, the `server_private_key_path` default) is
generated there and lost when the container is recreated — the code-layer
fix ANT-45 P2-1 (tracked in ANT-87) moves that key under `/data/ssl/`,
after which `/config` carries no durable state and the single-`/data`
layout becomes self-sufficient. Until then, treat a lost E2EE key as a
forced client re-pairing, not a data loss. The self-signed TLS certificate
already lives in the persistent tree (`/data/ssl/`). If you prefer the
reference layout, bind-mount a second volume at `/config` in
`docker-compose.yml` — the image pre-creates and owns the directory, so no
other change is needed.

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
