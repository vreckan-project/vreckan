# Cutover Runbook: SQLite → Postgres (v0.5.0 → v0.6.0)

The end-to-end procedure for cutting over a **running** vreckan deployment
from the v0.5.0 (Python) server on the default SQLite backend to the v0.6.0
(Rust) server on the bundled Postgres 16 backend, without re-seeding users,
groups, apps, and settings. See the [Ops & Deployment Guide](./ops.md) for
the deployment model and [release process](./release.md) for how the v0.6.0
image gets published.

## When to use this runbook

- Your deployment ran v0.5.0 (or earlier) with the default SQLite backend
  (the configuration database at `vreckan-data/vreckan.db` in the data root).
- You want the v0.6.0 deployment default: the `docker-compose.yml` in the
  v0.6.0 checkout starts a `postgres:16` service and points `vreckan-http`
  at it.
- The deployment runs from a compose checkout on a single host (the default
  model in the ops guide). Deployments that already ran a custom Postgres in
  v0.5.x only need the data move (steps 4–6), not the image/compose swap.

## At a glance

| # | Step | Duration |
|---|---|---|
| 0 | Prepare: v0.6.0 compose + image, Python + `psycopg2` on the host | a few minutes |
| 1 | Snapshot the SQLite database | seconds |
| 2 | Stop the legacy (v0.5.x) service | seconds |
| 3 | Start the Postgres service and wait for healthy | ~30 s |
| 4 | Migration dry-run | seconds |
| 5 | Migration (real run) | seconds–minutes |
| 6 | Verify row counts | seconds |
| 7 | Start the Rust service against Postgres | a few minutes (image pull) |
| 8 | Post-cutover health check + smoke test | a few minutes |
| 9 | Rollback (only if step 6–8 checks fail) | a few minutes |

**Expected downtime: roughly 10–15 minutes** (steps 2–8), most of it the
v0.6.0 image pull. The data move itself (steps 4–6) takes seconds for a
typical configuration database.

## Prerequisites

- **Compose checkout at v0.6.0.** The cutover uses the `docker-compose.yml`
  from the v0.6.0 checkout (it adds the `postgres` service and the
  `VRECKAN_DATABASE_URL` default). If your deployment runs from a git
  checkout, `git pull` to v0.6.0; otherwise copy the v0.6.0
  `docker-compose.yml` into your deployment directory.
- **The v0.6.0 image** — `ghcr.io/vreckan-project/vreckan:v0.6.0` is
  published on the tag push (ops guide §3.3). Point `image:` at the
  versioned tag, not `:latest`, so a later release can't change what you
  just cut over to. In a registry deployment drop the local `build:` block.
- **Python 3.8+ on the host** for the migration and verification scripts.
  `psycopg2` is needed for the *real* migration run and the row-count
  check: `pip install psycopg2-binary`. The dry-run and the snapshot are
  stdlib-only.
- **Know your Postgres credentials.** The bundled service uses
  `VRECKAN_DB_USER` / `VRECKAN_DB_PASS` / `VRECKAN_DB_NAME` from `.env`
  (defaults: `vreckan` / `vreckan` / `vreckan`). The commands below use the
  defaults; substitute your own if you customized them.
- **The SQLite path.** The legacy server's configuration database is at
  `vreckan-data/vreckan.db` in the compose directory (mounted as
  `/data/vreckan.db` inside the container). Confirm with
  `ls -la vreckan-data/vreckan.db`.

**DSN note.** The scripts run **on the host**, so they reach the bundled
Postgres through the published port: `postgres://vreckan:vreckan@localhost:5432/vreckan`.
The *container* (step 7) reaches it over the compose network as
`postgres://vreckan:vreckan@postgres:5432/vreckan` — the v0.6.0 compose
file already sets that for `vreckan-http`, so no `.env` change is needed
for the default credentials.

## Step 0 — Prepare

```bash
cd /path/to/vreckan            # the compose checkout
git pull                       # or: copy the v0.6.0 docker-compose.yml here
ls -la vreckan-data/vreckan.db # confirm the SQLite database exists
python3 -c "import psycopg2" || pip install psycopg2-binary
```

Set the image to the versioned v0.6.0 tag in `docker-compose.yml`:

```yaml
    image: ghcr.io/vreckan-project/vreckan:v0.6.0
    # remove the `build:` block in a registry deployment
```

Optionally pull it now so step 7 is fast:

```bash
docker compose pull
```

## Step 1 — Snapshot the SQLite database

```bash
python3 scripts/snapshot-sqlite.py \
    --src ./vreckan-data/vreckan.db \
    --dest-dir ./vreckan-snapshot
```

This writes `vreckan-snapshot/vreckan-<UTC-timestamp>.db` using SQLite's
online backup API (consistent even while the legacy service is still
running), runs `PRAGMA integrity_check` on the copy, and prints a
per-table row-count baseline. **Save that output** — it is the reference
for steps 6 and 9. Keep the snapshot file until the deployment has been
stable on Postgres for a few days.

Want a second, independent safety net? The admin UI's backup feature
(ops guide §4) produces a tar.gz of the whole data tree plus a logical dump
— worth taking right after the file snapshot, while the legacy service is
still up.

## Step 2 — Stop the legacy service

```bash
docker compose stop vreckan-http
docker compose ps
```

The v0.5.x service (whatever image tag it was pinned to) is stopped; the
data root is untouched. No app sessions survive the restart either way, so
this is the right moment.

## Step 3 — Start the Postgres service

With the v0.6.0 compose file in place:

```bash
docker compose up -d postgres
docker compose ps postgres     # wait until the state column says "healthy"
```

On first start, the `postgres:16` container initializes the `vreckan`
database with the user/password from `.env` and publishes port 5432 on the
host (the DSN in the scripts above). The healthcheck is
`pg_isready -U vreckan -d vreckan`; if it stays `starting` past a minute or
two, look at `docker compose logs postgres`.

## Step 4 — Migration dry-run

```bash
VRECKAN_SQLITE_PATH=./vreckan-data/vreckan.db \
python3 scripts/migrate-sqlite-to-postgres.py --dry-run
```

The dry run needs no Postgres connection: it prints the Postgres DDL it
would apply and the row counts it would move. Sanity-check the output:

- every populated table from your step 1 baseline appears with the same
  count;
- tables your deployment never used are listed as "not present in SQLite
  (will be created empty)" — expected and harmless;
- the total row count is what you expect (configuration databases are
  small; tens to low thousands of rows).

Fix surprises (a DB path pointing at the wrong file, a stale checkout)
before the real run — the dry run is the cheap moment to notice them.

## Step 5 — Migration (real run)

```bash
VRECKAN_SQLITE_PATH=./vreckan-data/vreckan.db \
VRECKAN_DATABASE_URL=postgres://vreckan:vreckan@localhost:5432/vreckan \
python3 scripts/migrate-sqlite-to-postgres.py
```

The script creates the schema (idempotently), then replaces each table's
contents in a single transaction and prints `Migration complete.` with the
per-table row counts. Properties that make this step low-risk:

- **the SQLite file is only read** — it is not modified;
- **one transaction** — a failure mid-migration rolls back everything the
  script did;
- **idempotent** — if it fails, fix the cause and re-run; the snapshot
  from step 1 is still your rollback.

## Step 6 — Verify row counts

```bash
VRECKAN_SQLITE_PATH=./vreckan-data/vreckan.db \
VRECKAN_DATABASE_URL=postgres://vreckan:vreckan@localhost:5432/vreckan \
python3 scripts/verify-cutover.py
```

This independently recounts every table in the original SQLite database and
in Postgres and prints a side-by-side table, exiting non-zero on any
mismatch. Expect every line to read `OK`, with totals matching the step 1
baseline. If a table differs, the migration transaction likely rolled back
or you pointed at the wrong database — fix and re-run step 5, then verify
again.

## Step 7 — Start the Rust service against Postgres

```bash
docker compose up -d
docker compose ps
```

The v0.6.0 compose file starts `vreckan-http` on the v0.6.0 image and only
after the `postgres` service is healthy (`depends_on: condition:
service_healthy`), with `VRECKAN_DATABASE_URL` pointed at the bundled
Postgres. If your `.env` customized the DB credentials, the same values are
used on both sides — the compose file interpolates them into both the
Postgres service and the server.

The first start creates the runtime state (TLS certs are reused from the
data root; the bootstrap-admin seeding is a no-op because users already
exist). Watch the startup:

```bash
docker compose logs -f vreckan-http
```

## Step 8 — Post-cutover health check + smoke test

```bash
curl -ks https://<your-server>/api/health
# → ok (N session(s))

python3 scripts/verify-cutover.py \
    --sqlite ./vreckan-data/vreckan.db \
    --postgres postgres://vreckan:vreckan@localhost:5432/vreckan \
    --health-url https://<your-server>/api/health
```

The script re-verifies the row counts and then GETs `/api/health`
(self-signed TLS accepted), exiting non-zero unless the server answers
HTTP 200.

Then smoke-test like a user:

1. Sign in with an existing account (the pre-cutover admin, and a regular
   user if you have one). Auth tokens were migrated, so previously issued
   tokens still work.
2. Check the admin UI: users, groups, roles, volume mounts, app stores and
   installed apps are all present.
3. Start a session on one app and confirm it behaves; then stop it.
4. Scan `docker compose logs vreckan-http` for errors — startup loads,
   cert selection, and any DB error surface there.

If all of that is green, the cutover is complete. Keep the snapshot from
step 1 until the deployment has run stably for a few days, then it can be
deleted. The Postgres data directory (`vreckan-postgres-data/`) is now
durable state: include it in your backup routine (the admin UI backups
already cover the database contents via the logical dump).

## Step 9 — Rollback (only if a step 6–8 check fails)

If verification, the health check, or the smoke test fails, revert to the
known-good state. Order matters: stop the new writer before touching the
data.

```bash
# 1. Stop the Rust service (the only thing that writes to the new Postgres).
docker compose stop vreckan-http

# 2. Optional: preserve what the Rust service wrote to Postgres during the
#    failed window, in case anything is worth keeping later.
docker compose exec postgres pg_dump -U vreckan vreckan \
    > vreckan-snapshot/postgres-dump-$(date +%Y%m%dT%H%M%S).sql

# 3. Point the deployment back at the legacy configuration:
#    - image: back to your v0.5.x tag (or the compose file from the v0.5.0
#      checkout, which has no postgres service)
#    - .env: remove or comment out VRECKAN_DATABASE_URL — both the legacy
#      server and the Rust server default to SQLite in the data root — or
#      set it explicitly to
#      VRECKAN_DATABASE_URL=sqlite+aiosqlite:////data/vreckan.db
#      (the in-container absolute path; the Rust server normalises that
#      SQLAlchemy-style URL, so the same value works on either server)

# 4. Restore the step 1 snapshot over the data-root database file. The
#    migration only *reads* SQLite, so the file is normally already
#    pristine — but restoring the snapshot makes the rollback exact and
#    covers any deployment that let the server write during the window.
cp vreckan-snapshot/<the-snapshot-from-step-1>.db vreckan-data/vreckan.db

# 5. Start the legacy service and confirm it is healthy.
docker compose up -d vreckan-http
curl -ks https://<your-server>/api/health
# → ok (N session(s))
```

The bundled Postgres service can be left running or stopped
(`docker compose stop postgres`); its data is now known to be a partially
migrated copy and is safe to discard later by removing the
`vreckan-postgres-data/` directory. Investigate the failure, re-run the
cutover from step 4, or stay on the restored configuration as long as you
need to.

## Troubleshooting

| Symptom | First look |
|---|---|
| `psycopg2 is required for the migration` | `pip install psycopg2-binary` on the host (dry-run and the snapshot don't need it). |
| Dry run reports "SQLite database not found" / zero rows | Wrong `VRECKAN_SQLITE_PATH` — the file is `vreckan-data/vreckan.db` in the compose directory, not inside the container. |
| Migration can't reach Postgres | `docker compose ps postgres` must be `healthy`; from the host the DSN is `postgres://…@localhost:5432/vreckan` (the `postgres` hostname only resolves inside the compose network). |
| `vreckan-http` won't start after `docker compose up -d` | It waits on the postgres healthcheck — check `docker compose logs postgres`, then `docker compose logs vreckan-http` for the server's own startup errors. |
| Health endpoint 200 but sign-in fails | Compare the `users` row count in step 6; check the bootstrap-admin log lines — a first-run seed only happens when no admin user exists, so a migrated deployment should not see one. |
| Session starts but the app misbehaves | The data root (`storage/`, `ssl/`, caches) is shared and untouched by the cutover; this class of failure is usually pre-existing — confirm against the legacy service's behaviour before blaming the move. |
