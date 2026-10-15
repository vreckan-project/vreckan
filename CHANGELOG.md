# Vreckan — Changelog

Vreckan is a hard fork of [SealSkin 0.3.2](https://github.com/linuxserver/docker-sealskin).
The full fork analysis — what was removed, rewritten, and added relative to the
upstream tree — lives in [`HISTORY.md`](./HISTORY.md). This changelog tracks
development *after* the fork, newest first. The Python-era entries below date
from the squashed git history of the original fork; the Rust-port era (2026-10)
is committed normally in `vreckan`. The changelog is the authoritative
record of what shipped in each release — see
[`docs/release.md`](./docs/release.md) for the conventions.

## 2026-10-15 — rsa 0.10 crypto-stack unification, v0.6.4 (ANT-191)

`rsa` bumped to **0.10.0-rc.19** — the first release built on the
pkcs8 0.11 / spki 0.8 / der 0.8 / rand_core 0.10 / sha2 0.11 stack —
removing the `pkcs8_010` and `rand_core_06` version-alias shims that
v0.6.3 carried to keep `rsa` 0.9 compiling alongside the newer pins.

- **`rsa` 0.9 → 0.10.0-rc.19, `sha2` 0.10 → 0.11, `spki` 0.7 → 0.8.**
  All three move to the same RustCrypto major generation as `pkcs8`
  0.11 / `der` 0.8 / `rand_core` 0.10 already on main.
- **Alias shims removed.** The `pkcs8_010` and `rand_core_06`
  `[dependencies]` aliases (and every `use` of them) are gone; the
  code now uses `pkcs8` / `rand` directly.
- **RNG unified on `rand::rng()`.** One-time server-key generation and
  the PSS-nonce draw now use the thread-local CSPRNG (seeded from OS
  entropy) instead of the separate `rand_core 0.6` `OsRng`.
- **API adjustments for the new `num-bigint-dig` / `digest` 0.11
  types** (`to_bytes_be` → `to_be_bytes_trimmed_vartime`, turbofish
  moved to type position for `Pss` / `Oaep`).
- **`VERSION` and `Cargo.toml` bumped to 0.6.4.**

## 2026-10-15 — pkcs8 0.11 + rand_core 0.10, v0.6.3 (ANT-128)

The two RustCrypto ecosystem bumps deferred in v0.6.2 (while `rsa` 0.10
was still pre-release) have now landed as separate renovate PRs. Each
keeps a backward-compatible version alias so the still-unbumped `rsa` 0.9
crate keeps compiling; no operator-visible behavior changes, and the full
412-test suite passes on both branches.

- **`pkcs8` 0.10 → 0.11** (PR #27). A `pkcs8_010` alias on `pkcs8` 0.10
  preserves the `rsa` 0.9 / `pkcs1` 0.7 trait interop; it is dropped when
  `rsa` 0.10 (which consumes `pkcs8` 0.11 directly) is released.
- **`rand_core` 0.6 → 0.10** (PR #28). A `rand_core_06` alias on `rand_core`
  0.6 preserves the `rsa` 0.9 `OsRng` interop; same exit condition.
- **`VERSION` and `Cargo.toml` bumped to 0.6.3.**

## 2026-10-15 — ACME account key moved to ECDSA P-256; acmex pinned to upstream (ANT-138)

Let's Encrypt production stopped accepting **EdDSA** (Ed25519) account
keys in October 2026 — it now accepts only RS256 / ES256 / ES384 /
ES512. The published `acmex` 0.8.0 hard-codes the account key as Ed25519,
so `Issue/Renew now` failed with a misleading "Missing location header"
(the account `newAccount` POST returned 400 before the header check).

- **ECDSA P-256 account key (ES256).** `load_or_create_account_key` now
  generates P-256 keys and **rotates** any persisted key of another type
  (the orphaned account is simply re-registered under the new key).
  ES256 is accepted by all three supported CAs.
- **`acmex` pinned to an `antitux/acmex` fork** of the upstream repo
  (houseme/acmex `f7b06c2`, 2026-10-06, + six fixes) via
  `[patch.crates-io]`: only upstream derives the JWS `alg` and JWK from
  the actual key (`Jwk::for_key_pair`, `JwsSigner::jwa_algorithm`) and
  ships `KeyPairGenerator`. The fork additionally (1) defaults the
  `contact` field in the account response — Boulder omits it for
  contact-less accounts, so a successfully registered account (2xx +
  Location) still failed to parse — (2) sends an **empty payload** on
  POST-as-GET requests (order/authz lookup, challenge trigger,
  certificate download, account lookup): Boulder rejects the `e30`
  (`{}`) encoding with "POST-as-GET requests must have an empty
  payload", which 400ed the authorization fetch, and (3) budgets
  **5 minutes per order-polling stage** (the old 60s was below Let's
  Encrypt's dns-01 validation cadence — the first validation attempt
  fails while the DNS change propagates and retries arrive minutes
  later, so a correctly configured issuance timed out with the order
  still pending) — (4) activates dns-01 challenges with an **empty
  object** (`{`) instead of an empty payload: per RFC 8555 §7.5.1 (and
  Boulder's `postChallenge`), an empty payload is a POST-as-GET that
  only *fetches* the challenge state, so the fork's uniform empty
  payload turned every activation into a no-op fetch and the
  challenges sat "pending" forever — the single most insidious layer,
  because the order looked healthy (authz pending, order pending, no
  errors) and Let's Encrypt's own status page said "Operational" — and
  (5) sets the **CSR subject CN** to the first identifier: rcgen's
  `CertificateParams::default()` subjects every CSR to
  `CN="rcgen self signed cert"`, which Boulder validates as a domain
  name and rejects at finalization ("Domain name contains an invalid
  character") — and (6) makes the **Cloudflare TXT create idempotent**:
  a *new* order for the same domain reuses the *existing* valid
  authorization, so the challenge token (and thus the digest content)
  is unchanged and Cloudflare's 81058 "An identical record already
  exists" fires when a previous run left the record in place; the
  provider now reuses the existing record's ID. The re-resolution also
  moved the acmex DNS/HTTP
  subtree to the advisory-fixed majors (h2 0.4, hickory 0.26,
  rustls 0.23, rustls-webpki 0.103, hyper 1.x), so the now-unnecessary
  RUSTSEC ignores were dropped from `deny.toml` and the acmex git source
  was added to its source allowlist.
- **CA directory URLs resolve through acmex's CA table** instead of
  hand-coded constants that had gone stale: ZeroSSL now targets its
  current EAB-gated `https://acme.zerossl.com/v2/DV90` endpoint (the
  old `/v2/server/directory` 403s) and Google targets
  `https://dv.google.com/acme/directory` (the old `acme.v1.pki.goog`
  404s). ZeroSSL additionally requires an External Account Binding
  (account key ID + MAC key), which vreckan does not yet configure —
  tracked as a follow-up.
- **Verified end-to-end on the staging box (192.168.103.225,
  `vreckan.antitux.net`, Cloudflare DNS-01, Let's Encrypt
  **production**).** Boot-time issuance and the admin "Issue/Renew
  now" button both complete in ~2 s: a real `Let's Encrypt YE1`
  certificate for `CN=vreckan.antitux.net` (SAN included, 90-day
  validity) is saved to the cert DB, hot-reloaded into the serving
  TLS config, and presented on `https://vreckan.antitux.net`.
  The earlier "pending forever" symptom was *not* a Cloudflare,
  DNSSEC (the `antitux.net` zone has no DS at `.net`), or
  Let's Encrypt incident — it was the activation semantics above,
  confirmed by activating a stale challenge by hand (validated in
  <15 s) and by reading Boulder's WFE source.

## 2026-10-15 — Native ACME client, certbot sidecar removed (ANT-128)

The certificate-issuance path is rewritten around the native **`acmex`**
Rust ACME client. The legacy `certbot/dns-cloudflare` Docker sidecar
(`src/le_certbot.rs`) is retired — `acmex` is now the **only** issuance
backend, and the Docker socket is no longer required for certificate
issuance (it remains in use for app containers and GPU setup).

- **`le_*` → `acme_*` rename.** Every configuration key, API field, and
  internal identifier that carried the legacy "Let's Encrypt" prefix is
  renamed to the generic `acme_` prefix (13 DB settings, the admin API
  request/response shapes, env vars, and the UI i18n keys). The feature
  is now documented as **ACME** rather than "Let's Encrypt".
- **Three CAs.** `acme_ca` selects the certificate authority:
  **Let's Encrypt** (default), **ZeroSSL**, or **Google Public
  Certificates**. The `acme_staging` flag still selects the Let's Encrypt
  staging endpoint for test certificates.
- **Eleven DNS-01 providers.** `acme_dns_provider` selects the DNS
  provider that manages the domain's zone: Cloudflare (default), AWS
  Route 53, DigitalOcean, Linode, Azure DNS, Google Cloud DNS, Alibaba
  Cloud, GoDaddy, Tencent Cloud, Huawei Cloud, and CloudNS.
  `acme_dns_provider_config` carries the JSON credentials for any
  non-Cloudflare provider; `acme_cloudflare_zone_id` optionally pins the
  Cloudflare zone.
- **Certificate stored in the config database.** The issued bundle
  (PEM chain + private key + expiry + CA + domains) is persisted in the
  `app_settings` table as the source of truth. A best-effort file mirror
  is written to `/data/acme/live/<primary>/` for backward compatibility;
  the old `/data/le/` certbot layout is retired.
- **Force-renew button.** `POST /api/admin/certificates/renew` now always
  (re)issues, even when a valid certificate already exists (the previous
  "already valid" short-circuit that made the button appear to do
  nothing is gone). The response carries `issued`/`renewed` plus
  human-readable `logs` and `error` fields so the UI can explain what
  happened.
- **Dynamic hot-reload.** After a successful issuance or renewal the TLS
  endpoint is reloaded in-place — no server restart is needed. The
  background renewal loop reads the effective config fresh on each
  iteration, so changes made on the admin page take effect without a
  restart.
- **Docker build-deps for acmex.** The `Dockerfile` build stage adds
  `cmake`, `make`, and `golang` (required by the FIPS variant of
  aws-lc, which acmex pulls in via `aws-lc-rs/fips` and builds from
  source through its CMake project). See the per-package justifications
  in the `Dockerfile`.

**Upgrading:** pull the new image and `docker compose up -d`. Existing
`VRECKAN_LE_*` env vars are read as `VRECKAN_ACME_*`; the DB
`app_settings` keys are migrated from `le_*` to `acme_*` on first
startup. The `/data/le/` directory is no longer used; the cert mirror
lives at `/data/acme/`.

## 2026-10-14 — Dependency bumps, v0.6.2 (ANT-118)

Batch dependency bump and version release. All bumps were QA-validated for
breaking changes before landing; no operator-visible behavior changes.
`VERSION` and `Cargo.toml` bumped to 0.6.2.

- **`aes-gcm` 0.10 → 0.11** (merged separately as PR #23 before this batch).
  The E2EE `Aes256Gcm` handshake API is unchanged; the removed 0.10-only items
  (`std`/`stream` features, `C_MAX`, `AeadInPlace`) are unreferenced in our code.
- **`axum-extra` 0.10 → 0.12.** We enable only the `cookie` feature; no
  `axum_extra` import exists in our Rust source (cookies are parsed/built
  manually in `src/auth.rs`), so the 0.12 breaking changes (opt-in features,
  `option_layer` body mapping, `async-stream` removal) do not apply.
- **`axum-server` 0.7 → 0.8.** The `tls_rustls::RustlsConfig::from_pem` and
  `bind_rustls` / `.serve(...)` call sites in `src/main.rs` are unchanged.
- **`base64` 0.22 → 0.23.** The `base64::engine::general_purpose` API
  (`STANDARD.encode/decode`, `URL_SAFE_NO_PAD.encode/decode`) is unchanged.
- **`reqwest` 0.12 → 0.13.** The feature flag name changed
  (`rustls-tls` → `rustls`); the `form` feature was added explicitly.
  The `default-features = false` + named-feature pattern is otherwise
  unchanged.
- **`rand` 0.9 → 0.10.** In 0.10 the `RngCore` trait was split: `fill_bytes`
  moved to a new `RngExt` sub-trait, and the unified `Rng` trait now
  re-exports both. All seven `use rand::RngCore` imports were updated to
  `use rand::Rng`; one additional `use rand::RngExt` was added where
  `fill_bytes` is called on the result of `rand::rng()`.
- **`der` 0.7 → 0.8.** Part of the RustCrypto ecosystem; the `der::Decode`
  import in `src/le_certbot.rs` is unchanged.
- **`x509-cert` 0.2 → 0.3.** The public-field access pattern
  (`cert.tbs_certificate.validity.not_after`) changed to method calls
  (`cert.tbs_certificate().validity().not_after()`); the single call site in
  `src/le_certbot.rs` was updated accordingly.
- **`rcgen` 0.13 → 0.14.** Used for the self-signed cert fallback in the
  Let's Encrypt / certbot flow; the API we call is unchanged.

**Deferred** (awaiting ecosystem releases): `rsa` 0.10 (currently pre-release
`0.10.0-rc.19`), which would in turn unblock `pkcs8` 0.11, `spki` 0.8,
`sha2` 0.11, and `der` 0.8 → 0.9. `jsonwebtoken` 10 → 11 is blocked on the
`jwks` crate ecosystem.

## 2026-10-14 — Post-release security fixes, v0.6.1 (ANT-80/81/87/88/89/90)

Post-release hardening of the v0.6.0 switchover, from the ANT-45 security
review. All eight fixes are merged to `main` and verified by QA (P1 wave on
ANT-82, P2 wave + full regression on ANT-91); no functional regressions. The
P3 findings from the review are deferred to a later release. `VERSION` and
`Cargo.toml` bumped to 0.6.1.

- **Non-root container (P1-1, ANT-81).** The server process now runs as a
  dedicated non-root user (`vreckan`, UID/GID 1000); the default
  `docker-compose.yml` drops `privileged: true`. It still binds 443 (via a
  `cap_net_bind_service` file capability on the binary) and reaches the host
  Docker daemon through the host `docker` group instead of blanket
  privileges. A one-shot `data-perms` service chowns the `vreckan-data`
  volume to UID/GID 1000 on every `docker compose up`, so no manual `chown`
  is needed on fresh installs or when upgrading from v0.5.x. Set
  `VRECKAN_DOCKER_GROUP` if your daemon's socket group is not named `docker`.
- **First-boot admin must change password (P1-2, ANT-80).** The bootstrap
  `admin` account is now flagged *must-change-password*, so the web client
  prompts for a new password at first sign-in. The first-run log no longer
  prints a *configured* bootstrap password (only the well-known `admin1234`
  default). A new `VRECKAN_DEFAULT_PASSWORD_POLICY` (`allow` | `warn`
  (default) | `reject`) controls how a login with the literal `admin1234` is
  treated while that flag is still set.
- **E2EE server key persisted (P2-1, ANT-87).** The E2EE server RSA key now
  defaults to the persistent data root — `VRECKAN_SERVER_PRIVATE_KEY_PATH` is
  `/data/ssl/server_key.pem`, written `0600` — instead of the ephemeral
  `/config/ssl/`. It now survives container recreation; previously a
  re-created container generated a fresh key and forced every client to
  re-pair.
- **Cloudflare ACME token written 0600 (P2-2, ANT-87).** The Cloudflare
  DNS-01 token file the server writes for certbot
  (`/data/le/cloudflare.ini`) is now tightened to `0600`, so the plaintext
  token is owner read/write only.
- **Migration script masks DSN credentials (P2-3, ANT-88).**
  `scripts/migrate-sqlite-to-postgres.py` now masks the `user:pass` userinfo
  when it prints the target DSN and scrubs it out of connection-error
  tracebacks, so credentials no longer leak into logs.
- **Backup hygiene (P2-4, ANT-88).** Config-DB backup archives are written
  `0600`; the in-archive database dump excludes live session tokens
  (`auth_tokens`) so a restore leaves in-flight logins untouched; and the
  default archive retention changes from keep-all to 7
  (`VRECKAN_BACKUP_RETENTION`).
- **Login rate limiting (P2-5, ANT-89).** `POST /api/auth/login` now
  rate-limits per (client IP, username): `VRECKAN_LOGIN_MAX_ATTEMPTS`
  (default 5) failures within `VRECKAN_LOGIN_LOCKOUT_SECONDS` (default 900 s)
  lock the pair out with HTTP 429 for the same window (fail closed). A
  successful login clears the pair; `0` disables the limiter.
- **Postgres loopback-only + default-creds guard (P2-6, ANT-90).** The
  bundled Postgres service is now published on the host loopback interface
  only by default (`127.0.0.1:5432`) — the server reaches it over the
  internal compose network either way; set `VRECKAN_POSTGRES_PORT=5432` to
  expose it on every interface again. Starting against the default
  `vreckan`/`vreckan` credentials logs a warning by default;
  `VRECKAN_ALLOW_DEFAULT_DB_CREDS` (`1` to silence, `refuse` to fail startup)
  controls the guard.

**Upgrading:** pull the new image and `docker compose up -d` — the
`data-perms` service handles ownership automatically. If you rely on the old
`/config/ssl` E2EE key location or the previous SQLite-as-compose-default
setup, review [docs/ops.md](./docs/ops.md) and the
[configuration reference](./README.md).
- **Core Rust fixes from the `ant-30` branch (ANT-30) merged into the
  v0.6.0 tree.** Swallowed database-write errors in the global-settings,
  SSO-config, and template-schema saves now surface as 500 instead of
  silently reporting success; E2EE requests with a mis-sized IV get a 400
  instead of an index panic; and the files listing validates its pagination
  bounds (400) instead of indexing out of range.
- **Python-era leftovers removed from the tree.** The root
  `requirements.txt` and `pytest.ini` belonged to the FastAPI server (now in
  the `vreckan-python` archive repo) and the old pytest suite (now a Rust
  integration test), so both are gone. `server/` stays: the image ships
  `server/static` (web client) and `server/template_schema.yml` (template
  editor baseline). The "no Python in the image" claim now matches the tree.
- **`scripts/migrate-sqlite-to-postgres.py` finalized** (it is the only
  Python left in the repo). Its table order now matches `db.rs`
  `TABLE_ORDER`, and its generated DDL now uses the exact column types the
  server's own schema sync creates on a fresh Postgres (`varchar`/`json`,
  not `text`/`jsonb`), so a migrated database boots with no schema-drift
  warnings. It remains idempotent (safe to re-run), and `--dry-run` still
  previews the DDL and row counts with the stdlib alone. A real run needs
  `psycopg2-binary` — see docs/ops.md §4.1. Verified end-to-end against a
  live `postgres:16` with a real v0.6.0 SQLite database.

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
  GitHub Actions workflows are disabled there). Only the web client
  (`server/static/`) and the bundled template schema
  (`server/template_schema.yml`) remain of the old tree; the Python-era
  root files `pytest.ini` / `requirements.txt` no longer exist (the one
  remaining Python file is the migration tool in `scripts/`).
- **Final Rust deployment image + CI/CD (ANT-41/ANT-43/ANT-57).** The
  `Dockerfile` is multi-stage: a `rust:1.99-slim` build stage (sccache-wrapped
  `rustc`, BuildKit cache mounts), a test stage that pre-compiles
  `cargo test --profile test-release`, and a `debian:trixie-slim` runtime
  (no Python in the image). CI is the GitHub-light/Gitea-heavy pattern:
  `gitea-mirror-trigger.yml` mirrors main/PRs to the Gitea mirror and
  dispatches `build-test-rust.yml` on the self-hosted runner; the `v0.6.0`
  tag push fires the Gitea Release workflow, which publishes
  `ghcr.io/vreckan-project/vreckan:v0.6.0` alongside `:latest` (and to the
  Gitea registry). See [docs/ops.md §3](./docs/ops.md).
- **Docs finalized for the v0.6.0 state (ANT-47).** `README.md`,
  `docs/ops.md`, the `site/docs/` project-site pages, and the
  `HISTORY.md` cross-references were reconciled with the final switchover
  state (Postgres-default compose, final CI/CD flow, the `vreckan-python`
  archive); `docs/api.md`, `docs/release.md`, and `docs/port-status.md`
  were checked and needed no changes.

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
