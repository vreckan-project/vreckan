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
single listener.

## 2. Quick start

```bash
git clone https://github.com/vreckan-project/vreckan.git
cd vreckan
cp .env.example .env    # edit to taste (OIDC, ACME, ...)
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
and a successful login clears the pair. Counters are in-memory only, so a
server restart resets them.

The client IP here is the **direct TCP peer** unless you name your reverse
proxy: `X-Forwarded-For` is honoured only when that peer matches
`VRECKAN_TRUSTED_PROXIES` (see §5), because a client that reaches the origin
port directly can otherwise rotate the header to dodge the limiter. Behind an
unnamed proxy every client therefore shares one bucket — set
`VRECKAN_TRUSTED_PROXIES` to the proxy's IP or CIDR (e.g.
`10.0.0.0/8, 192.168.1.5`; malformed entries never match, so a typo fails
closed rather than opening the gate).

### 2.1 CORS (ANT-653)

The classic "login doesn't work" report: the user signs in at
`https://myserver.local/`, then a page they use reaches the **same server
under a different name** — the LAN IP (`http://192.168.1.10:443/`), a second
hostname, or a different port. Browsers treat each `scheme + host + port` as
a different *site*, so the login cookie (which is what the API authenticates
with) is never sent to the other alias, and every call comes back
"unauthenticated" — and before v0.9.5 there were no CORS headers at all to
let the cross-origin page through in the first place.

Since v0.9.5 the server ships a CORS layer with a mode switch:

- **`VRECKAN_CORS_MODE=loose`** (the default) — a request carrying an
  `Origin` gets it echoed into `Access-Control-Allow-Origin` together with
  `Access-Control-Allow-Credentials: true` and `Vary: Origin`, and OPTIONS
  preflights are answered directly (methods and headers including
  `X-Session-ID` and `Content-Type`). The login cookie is issued
  `SameSite=None; Secure` in this mode so it actually crosses to the other
  alias. The cross-origin-relevant security headers are relaxed to match:
  the CSP's `connect-src` widens to `https: http: ws: wss:`,
  `Cross-Origin-Opener-Policy` becomes `same-origin-allow-popups`, and
  `Cross-Origin-Resource-Policy` becomes `cross-origin`.
- **`VRECKAN_CORS_MODE=strict`** — no CORS headers at all, exactly the
  pre-v0.9.5 behaviour: only same-origin requests are accepted and the
  cookie stays `SameSite=Lax`.

**The trade-off (and the mitigations).** CORS-with-credentials is the
classic footgun: with a fully open `loose` configuration, any site already
open in the admin's browser can make credentialed calls to the API. Two
things bound the exposure: the **E2EE request body remains the trust anchor
for every state-mutating route** (a cross-site form POST or a page that
never completed the E2EE handshake is rejected before it mutates anything —
the same gate that has protected `join_my_session` since ANT-444), and the
optional allowlist below. If the machine holding the admin session is
trusted (the normal single-admin deployment), loose is safe as-is; if you
want to tighten it, set **`VRECKAN_CORS_ALLOWED_ORIGINS`** to a
comma-separated origin list (e.g. `https://a.example.com, https://b.example
.com`) — in loose mode only listed origins (compared case-insensitively)
receive the CORS headers, everything else is treated as in strict mode.
Comparison is exact after lowercasing; there are no wildcards.

Both settings are editable on the admin UI's Settings page (CORS card), and
the effective mode is re-resolved on every request, so a change applies to
the next request without a restart. Precedence is DB row over environment
variable over the code default (`loose`, empty allowlist).

**Interaction with `VRECKAN_TRUSTED_PROXIES` (§5).** `SameSite=None` is only
valid with `Secure`, and the cookie's `Secure` flag follows the *effective*
scheme — which behind a TLS-terminating proxy is whatever
`X-Forwarded-Proto` reports (trusted-proxy gate, §5). If that scheme is
plain `http`, the cookie falls back to `SameSite=Lax` even in loose mode so
it stays usable; the CORS headers are still emitted, but browsers will
reject a `SameSite=None` cookie without `Secure`, so for cross-alias
logins behind a proxy the proxy must report `https`.

Local build instead of pulling the image:

```bash
docker build -t vreckan .   # multi-stage: pinned vreckan-base-build image → debian:trixie-slim runtime
```

## 3. Image build, registries, and the CI/CD flow

### 3.1 The image

The `Dockerfile` is multi-stage:

- **Build stage** — the pinned `repo.vreckanproject.com/vreckan/vreckan-base-build`
  image (built on `rust:1.99-slim`, Debian trixie; see §3.5 — it bakes the
  toolchain, the `-sys` build deps, sccache, and a warm dependency cache):
  `cargo build --release` with BuildKit cargo-cache mounts.
- **System deps in the base image** — `linux-libc-dev`,
  `pkg-config`, `libssl-dev` (needed by the `-sys` crates and the native-tls
  stack) plus `cmake`, `make`, and `golang` (needed by the FIPS variant of
  aws-lc, `aws-lc-fips-sys`, which acmex pulls in via `aws-lc-rs/fips` and
  builds from source through its CMake project — its FIPS build requires Go
  for the FIPSTools integrity utilities and Perl, which ships with the rust
  base image) and `sccache`, which wraps `rustc` (`RUSTC_WRAPPER=sccache`)
  and persists the compile cache across builds via a BuildKit `type=cache`
  mount. See the per-package justifications in the `Dockerfile`.
- **Test stage** — the build stage plus the `tests/` and `server/` trees: CI
  builds `--target test` and pre-compiles `cargo test --profile test-release`
  (thin-LTO, codegen-units=16) so the Gitea test job only executes the tests.
  sccache serves unchanged dependency crates from its persistent cache, and the
  job count scales with the runner's core count and a per-rustc memory budget.
- **Runtime stage** — `debian:trixie-slim`: the compiled `vreckan` binary,
  the web client (`server/static/`), and the bundled template schema
  (`server/template_schema.yml`). No Python, no pip. TLS, SQLite, and the
  client are all self-contained.

### 3.2 Standard local build & test (for engineers)

Build and test **inside the standard image**, not a bare local toolchain —
it is the same container CI and the deploy use (verified for the full acmex
feature set: 11 DNS-01 providers + Let's Encrypt/ZeroSSL/Google CAs,
ANT-128/ANT-152). A hand-installed local `gcc`/`cmake` setup drifts from it.

```sh
# One command builds the whole tree (build stage) and pre-compiles the test
# binaries (test stage) in the standard container:
docker build --target test -f Dockerfile -t vreckan:local .

# Run the full suite in it, the same way the Gitea CI test job does:
HOST_TMP=$(mktemp -d /tmp/vreckan-tests.XXXXXX)
docker run --rm \
  --shm-size=2gb \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v "$HOST_TMP:/tmp/vreckan-tests" \
  -e VRECKAN_TEST_TMPDIR=/tmp/vreckan-tests \
  --entrypoint cargo vreckan:local test --profile test-release
rm -rf "$HOST_TMP"
```

The three run flags mirror the CI test job: the docker socket lets the
daemon-dependent tests (Tier 5 docker launch, the postgres fixture) reach a
real daemon; `VRECKAN_TEST_TMPDIR` must point at a directory **visible to
that daemon** — the server bind-mounts the test's throwaway data dirs into
the app containers it launches, and the daemon resolves the source on its
own host (the test container's own `/tmp` is not one); `--shm-size=2gb`
keeps the LTO compile and the postgres fixture off the 64MB default
`/dev/shm`.

For fast source iteration without the test tree, build only the build stage
and run it with your `src/` mounted in:

```sh
docker build --target build -f Dockerfile -t vreckan:build .
docker run --rm -it -v "$PWD/src:/app/src" vreckan:build \
  sh -c 'cargo build --release'
```

The deploy (docker-compose `build: .`) and the CI PR gate build the same
`Dockerfile` stages, so anything that builds here builds everywhere (§3.3,
§3.6).

### 3.3 The vLLM-style split (GitHub light, Gitea heavy)

CI follows the vLLM/Gitea pattern: GitHub Actions do the cheap parts, a
self-hosted **Gitea Actions runner** does the heavy build/test/scan work.
GitHub is the source of truth; Gitea is a mirror. Never push directly to
the Gitea `vreckan/vreckan` mirror — feature branches go to GitHub only, and
`gitea-mirror-trigger.yml` is the sole Gitea writer (`pr/<N>` branches;
`bump-and-tag` for main/tags).

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
  and the **nightly scan** workflow (§3.5). See ANT-57 for the staged
  enablement of this flow on the Rust port.

### 3.4 Registries

- **GHCR** — `ghcr.io/vreckan-project/vreckan` is the registry; every release
  publishes a versioned tag (`:v<VERSION>`) alongside `:latest`, and the
  compose file's `image:` tag points at it (Rust cutover complete, ANT-40).
- **Gitea registry** — the Release workflow also pushes to the self-hosted
  Gitea package registry for air-gapped/on-prem pulls.
- The compose file's `image:` tag is set by the cutover decision; `build: .`
  always builds the Rust image locally when the tag is not present.

### 3.5 Base-image pinning (`.base-image-digest`)

`.base-image-digest` records the digest of the **build** base image
(`repo.vreckanproject.com/vreckan/vreckan-base-build`, the self-contained
build/work image built on `rust:1.99-slim`), and the `Dockerfile` build
stage pins that same digest — the `FROM` line references
`vreckan-base-build@<digest>`, not the mutable `:latest` tag, so a re-bake
cannot swap the compiler toolchain under an image build without a visible,
reviewable diff (ANT-277). The two copies stay in lockstep: the file is the
tracked record, the `FROM` line is what every build (CI, local, compose
`build: .`) actually pulls. The image is built and
maintained out of the **vreckan-workflow** repo — its Dockerfile lives at
[images/vreckan-base-build/](https://repo.vreckanproject.com/vreckan/vreckan-workflow/src/branch/main/images/vreckan-base-build)
and the `build-base-build` Gitea workflow there builds + pushes it on demand
and re-bakes it daily when vreckan's `Cargo.lock` or the Rust base digest
moves (ANT-203). The nightly
scan workflow compares the live registry digest of that base against the
`vreckan.base-digest` label of the last nightly image; when they differ it
opens a bot PR re-pointing both `.base-image-digest` and the `Dockerfile`
build-stage digest pin (and bumping the patch version). The runtime base
(`debian:trixie-slim`) is a fixed tag and is not part of the digest
convention. When a digest re-point lands, the release notes should mention
the base-image bump (see [release process](./release.md)).

### 3.6 Self-hosted Gitea runner

The Gitea runner that executes `build-test-rust.yml` / Release needs:

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
| `postgres` (compose default) | nothing — the default compose file presets `VRECKAN_DATABASE_URL` to the bundled `postgres` service (`postgres:16`, user/db `vreckan`, password `vreckan`). For your own Postgres, set `VRECKAN_DB_BACKEND=postgres` + `VRECKAN_DB_HOST` / `VRECKAN_DB_PORT` / `VRECKAN_DB_USER` / `VRECKAN_DB_PASS` / `VRECKAN_DB_NAME`, or simply override `VRECKAN_DATABASE_URL` in `.env` | recommended for production; the default credentials in the compose file should be changed |
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
Postgres `json`, the type the server itself uses). It is safe to re-run. The server should be stopped during the
cut-over. It is the only remaining Python in the repository (the server
itself is a single Rust binary with no Python in the image), and it needs
Python 3.8+ (stdlib `sqlite3`) and `psycopg2`
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
- **ACME (optional).** Enable via env (`VRECKAN_ACME_*`) or the admin
  UI Certificates page (DB value overrides env). Issuance is handled by the
  **native `acmex` Rust client** — no external container is required, so
  the Docker socket is no longer needed for certificate issuance (it is
  still used for app containers and GPU setup).
  - `VRECKAN_ACME_ENABLED=true`,
    `VRECKAN_ACME_DOMAINS=example.com,www.example.com`
  - **CA** — `VRECKAN_ACME_CA` selects the certificate authority:
    `letsencrypt` (default), `zerossl`, or `google` (Google Public
    Certificates). `VRECKAN_ACME_STAGING=true` selects the Let's Encrypt
    staging endpoint for test certificates.
  - **Challenge type** — `VRECKAN_ACME_AUTH` selects the ACME challenge:
    `dns-cloudflare` (default; DNS-01 via Cloudflare, works behind a
    reverse proxy) or `http-01` (the ACME server fetches from your domain
    over HTTP).
  - **DNS provider** (DNS-01) — `VRECKAN_ACME_DNS_PROVIDER` selects the
    DNS provider that manages your domain's zone. Eleven providers are
    supported: `cloudflare` (default), `route53`, `digitalocean`, `linode`,
    `azure`, `google`, `alibaba`, `godaddy`, `tencent`, `huawei`, and
    `cloudns`.
  - **Provider credentials** —
    `VRECKAN_ACME_CLOUDFLARE_EMAIL` +
    `VRECKAN_ACME_CLOUDFLARE_TOKEN` for Cloudflare; or
    `VRECKAN_ACME_DNS_PROVIDER_CONFIG` (a JSON object with the
    credentials for any other provider).
    `VRECKAN_ACME_CLOUDFLARE_ZONE_ID` optionally pins the Cloudflare zone.
  - **Auto-renewal.** `VRECKAN_ACME_AUTO_RENEW=true` enables background
    renewal: the server checks every
    `VRECKAN_ACME_RENEW_CHECK_HOURS` hours and renews within
    `VRECKAN_ACME_RENEW_BEFORE_DAYS` days of expiry. The renewal loop
    reads the effective config fresh on each iteration, so changes made on
    the admin page take effect without a container restart.
  - **Certificate storage.** The issued certificate (PEM chain + private
    key + expiry) is persisted in the **config database** as the source of
    truth; a best-effort mirror is written to `/data/acme/live/<primary>/`
    for backward compatibility. Until a valid ACME certificate exists, the
    server falls back to the self-signed one.
  - **Hot-reload.** After a successful issuance or renewal, the TLS
    endpoint is reloaded in-place — no server restart is needed.
- **Behind a TLS-terminating reverse proxy (optional).** The server
  terminates TLS itself by default (the ACME flows above assume that). If you
  do terminate TLS at a proxy in front of it, set
  `VRECKAN_TRUSTED_PROXIES` to that proxy's IP or CIDR — comma-separated,
  e.g. `10.0.0.0/8, 192.168.1.5`. Only when the **direct TCP peer** matches
  the list are `X-Forwarded-Proto` and the leftmost `X-Forwarded-For` entry
  honoured. `X-Forwarded-Proto` drives the session cookie's `Secure` flag and
  the scheme of the OIDC `redirect_uri` when that URI is derived from the
  request rather than set explicitly (`VRECKAN_OIDC_REDIRECT_URI`); otherwise
  the scheme is the real connection scheme, which is always `https` here. The
  default empty value trusts no proxy — the safe posture for a directly
  exposed origin, and it also keeps the login limiter from keying on the
  proxy's address (§2), which is the usual reason to set this. Malformed
  entries never match, so a typo fails closed. This is an environment setting
  only: no admin-UI field, and it appears in no admin-settings payload (see
  the API reference).

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
  containers, plus the host-side parts
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
  and discovers its own published port
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
| `/var/run/docker.sock` | the server talks to the Docker Engine API to create/inspect/stop app containers — raw socket by default (see the strategy paragraph above) | required unless you run a dedicated provider; the server degrades gracefully at boot if the socket is missing or inaccessible (logs a warning, app launches and self-inspection are disabled) |
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

**Volume layout (v0.6.1, ANT-81 / ANT-87).** This deployment persists state
in a single `/data` bind mount (`./vreckan-data`). That is a deliberate
deviation from the SealSkin reference, which bind-mounts *both* `/config`
and `/storage` and keeps the E2EE server key at `/config/ssl/`. Since
v0.6.1 the E2EE server key defaults to the persistent data root —
`VRECKAN_SERVER_PRIVATE_KEY_PATH` is `/data/ssl/server_key.pem`, written
`0600` (ANT-45 P2-1) — so it survives container recreation and no longer
forces a client re-pair when the container is re-created. `/config` is now
a purely **ephemeral, image-owned** directory inside the server container
and carries no durable state, so the single-`/data` layout is
self-sufficient. The self-signed TLS certificate also lives in the
persistent tree (`/data/ssl/`). If you prefer the reference layout,
bind-mount a second volume at `/config` in `docker-compose.yml` — the image
pre-creates and owns the directory, so no other change is needed.

```
/data
├── vreckan.db                 SQLite database (sqlite backend)
├── storage/
│   ├── <user home dirs>       per-user home directories served to apps as /config
│   ├── vreckan_uploads/       temporary chunked-upload staging
│   ├── vreckan_app_icons/     custom app icons
│   ├── vreckan_home_templates/ meta-app home-dir templates
│   └── vreckan_public/        publicly shared files
├── ssl/                       TLS certs: self-signed (vreckan_dev.*); also the E2EE server key (server_key.pem, 0600)
├── acme/                      ACME cert mirror (best-effort; the config DB is the source of truth)
├── autostart_cache/           cached autostart scripts
├── app_stores_cache/          cached app-store YAML files
└── backups/                   backup archives (0600)
```

## 8. Observability

- `GET /api/health` — liveness probe (`ok (N session(s))`); point load
  balancers/monitors at it.
- Logs: `tracing`-based, structured. `RUST_LOG`, when set, takes precedence;
  otherwise `VRECKAN_LOG_LEVEL` sets the filter (both unset → `info`); SQL
  query logging is available via the SeaORM `sqlx_logging_level` hook.
- First-run credentials, cert selection, and startup loads are all logged —
  `docker compose logs -f vreckan-http` is the primary diagnostic surface.
