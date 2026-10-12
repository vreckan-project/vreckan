# Vreckan → Rust: Rewrite Evaluation

Independent assessment of rewriting the Vreckan **server** (currently Python /
FastAPI / uvicorn, ~12,211 LOC across 21 files) to **Rust**. The web client
(~6,758 LOC of framework-free JS + 16 i18n bundles) is served as static files
and does not need to change. It only talks to the server over HTTP/WS and does
its own E2EE crypto in the browser via WebCrypto.

## TL;DR

**Feasible: YES.** Every server capability has a solid Rust equivalent. The
hardest parts (the session reverse-proxy data plane, Docker GPU orchestration,
and OIDC) are all solvable with mature, actively-maintained crates, though they
carry more integration friction than the more mechanical endpoints. Rust is the
right choice if you value memory safety, a tiny static binary, and
compile-time-checked SQL.

**Estimated effort:** ~10–14k LOC of Rust. Roughly 6–10 weeks of focused work
for one experienced Rust dev to reach feature parity, dominated by the proxy,
Docker, and the ~100 admin/API endpoints.

---

## 1. What the server actually does

Two planes on one HTTPS endpoint:

- **Control plane (API):** ~100 endpoints. Auth (password + OIDC/SSO), RBAC,
  app catalog, launch orchestration, file manager, uploads, public shares,
  collaboration rooms, admin management, Let's Encrypt, backup/restore.
  Management traffic is **end-to-end encrypted** (E2EE).
- **Data plane (session proxy):** reverse-proxies
  `/api/apps/session/{id}/...` (HTTP **and** WebSocket) to the per-session app
  container, so containers are never exposed to the internet.

Durable state lives in a database (SQLite by default, Postgres optional): 13
tables, mostly `key + JSON blob`. Runtime state is held in **module-level
global dicts** guarded by `asyncio.Lock`.

## 2. Module-by-module mapping

| Python module (LOC) | Responsibility | Rust equivalent | Difficulty |
|---|---|---|---|
| `api.py` (3446) | App factory, E2EE handshake, `EncryptedRoute`, session proxy (HTTP+WS), launch orchestration, all global state | `axum` app + `tokio`; split into `state.rs`, `proxy.rs`, `handshake.rs`, `launch.rs` | **High** (god module) |
| `routers/admin.py` (1907) | 61 admin endpoints | `axum` routers | Medium (volume) |
| `user_manager.py` (1213) | users/groups/roles, argon2, effective settings/permissions | `argon2` crate + structs | Medium |
| `collaboration.py` (909) | multi-user rooms, WS fan-out, token broadcast to downstream (Selkies) | `axum` WS + `tokio` tasks | **High** |
| `routers/auth.py` (725) | password login, OIDC authorization-code flow, JWKS id_token verify | `jsonwebtoken` + manual OIDC (or `oidc-client`) | **High** |
| `models.py` (607) | Pydantic request/response models | `serde` + `serde_json` | Low |
| `db.py` (440) | SQLAlchemy async ORM, 13 tables, SQLite+Postgres | **`sqlx`** (async, both backends, compile-time checked) | Low–Med |
| `routers/files.py` (430) | file mgmt, chunked download, public shares | `axum` + `tokio::fs` | Low–Med |
| `settings.py` (418) | 47 settings, env + DB override precedence | `config`/manual + `sqlx` | Low |
| `providers/docker_provider.py` (393) | Docker SDK: pull (streaming), run (GPU/DRI3/group_add), health checks | **`bollard`** (async, Unix socket) | **High** |
| `le_certbot.py` (373) | transient certbot container, x509 parse, renewal loop | `bollard` + `x509-cert`/`rustls-pemfile` | Medium |
| `backup_manager.py` (328) | tar.gz of /data + JSON DB dump, safe restore | `tar` + `flate2` | Low |
| `routers/encrypted.py` (246) | E2EE catalog + launch endpoints | `axum` + `EncryptedRoute` analog | Low–Med |
| `routers/session.py` (192) | session list/status/stop/recreate/send_file | `axum` | Low |
| `routers/upload.py` (134) | chunked upload | `axum` + `tokio::fs` | Low |
| `routers/pinned.py` (122) | saved launch presets | `axum` | Low |
| `permissions.py` (97) | RBAC catalog + expansion | plain structs/sets | Low |
| `volume_mount_manager.py` (98) | volume mounts CRUD | `axum` + `sqlx` | Low |
| `providers/base_provider.py` (60) | provider trait | Rust `trait Provider` | Low |
| `routers/homedir.py` (50) | home dir CRUD | `axum` | Low |
| `logging_config.py` (23) | logging setup | `tracing` + `tracing-subscriber` | Low |

## 3. The hard parts, in detail

### 3.1 Session reverse-proxy (data plane): the hardest part
`api.py::_proxy_session_request` / `_proxy_session_websocket` must:
- Validate a per-session cookie, then forward the request **with the URL
  prefix intact** to `http://<container-ip>:<port>`.
- **Stream the raw (still content-encoded) response body.** It must not be
  decompressed, or the client double-decodes (`ERR_CONTENT_DECODING_FAILED`).
- Drop hop-by-hop headers, rewrite `Host`, optionally inject Basic auth.
- For WebSockets: drop the client's `Sec-WebSocket-*` headers (the client lib
  generates its own), negotiate subprotocols, and pump bytes both directions
  until either side closes.

**In Rust:** use `axum` + `hyper`. For HTTP, a manual streaming proxy
(`hyper::Client` with `body: Body::wrap_stream`) is straightforward but you
must be careful to pass raw bytes through and copy headers exactly. For WS,
`tokio-tungstenite` for the upstream leg and `axum::extract::ws` for the
client leg, with two `tokio::select!` pump tasks. **This is the single most
error-prone area.** The Python code has many subtle comments about header
duplication, subprotocol echo rules (RFC 6455), and raw-vs-decoded bodies.
Budget real time here and port the comments verbatim.

> **Crate note (verified on crates.io):** `axum-reverse-proxy` (v2.2.0,
> ~249k downloads, actively maintained) is a ready-made, flexible reverse
> proxy for Axum and can absorb much of the HTTP-leg plumbing (header
> rewriting, streaming). It does not by itself solve the *raw, still
> content-encoded body* requirement or the per-session cookie validation, so
> you'll still wrap it, but it cuts the boilerplate. `tokio-tungstenite`
> (v0.30, the canonical async WS client/server) is confirmed as the right
> upstream leg.

### 3.2 Docker orchestration
`docker_provider.py` uses the `docker` Python SDK via `asyncio.to_thread`:
image pull with **streaming progress callbacks**, `containers.run` with
`device_requests` (NVIDIA), raw `/dev/dri` + `/dev/nvidia-modeset` devices,
`group_add` propagation (DRI3 render-node access), network IP discovery, and a
readiness/health-check loop (including a collaboration control-plane probe).

**In Rust:** `bollard` is async and speaks the Docker Engine API over the Unix
socket. It covers pull (streaming), run, inspect, and device requests. The
NVIDIA `DeviceRequest` and `group_add` map cleanly to `bollard`'s
`HostConfig`. The health-check loop is just `tokio` polling. It is slightly
less battle-tested than the Python SDK for the exotic GPU cases, so expect to
read the Engine API spec for the device-request JSON.

### 3.3 OIDC / SSO: a smaller gap than it first looks
`routers/auth.py` implements the authorization-code flow: discovery document
fetch + cache, `state` nonce, token exchange, and **JWKS-based RSA
verification of the `id_token`** (with a manual JWK→PEM helper).

**In Rust:** this is *less* of a gap than the ecosystem's reputation suggests.
- **`openid`** (kilork), the de-facto standard, is an async/await **OpenID
  Connect + Discovery client** (v0.25.0, ~4.5M total downloads, updated
  2026-10). It handles discovery, the authorization-code flow, and `id_token`
  verification, built on `reqwest`. This is the direct replacement for most of
  the hand-rolled `_oidc_*` helpers.
- **`axum-oidc-client`** / **`axum-oidc-layer`** are Axum-specific OIDC/OAuth2
  middleware (both actively maintained in 2026) if you want the auth flow
  wired in as an Axum layer rather than manual routes.
- **JWKS:** the `jwks` crate (v0.5.3) and `jwk-simple` (v0.5.0, RFC 7517/7518
  /7638/8037/9864, integrates with `jwt-simple`) parse JWKS and build keys,
  so the manual JWK→PEM step can be replaced. `jwks-cache` adds async caching
  with ETag revalidation if you want it.

So the realistic path is `openid` (or `axum-oidc-client`) + `jwks`/
`jwk-simple` + `jsonwebtoken` for the remaining claim checks. The friction is
now mostly *integration* (mapping Vreckan's admin-UI-editable issuer/client
settings onto the crate's config) rather than reimplementing the protocol.

### 3.4 E2EE crypto (straightforward)
- Server identity: **RSA-PSS** sign a nonce (SHA-256, salt 32).
- Key wrap: **RSA-OAEP** (SHA-256) decrypt the client's AES key.
- Body crypto: **AES-256-GCM** (12-byte IV) on request/response JSON.

**In Rust:** `ring` (or `aws-lc-rs`) covers RSA-PSS, RSA-OAEP, and AES-GCM.
The client uses WebCrypto with the same parameters, so the wire format is
fixed. Just match it. Low risk.

### 3.5 Module-level global state → Rust
The Python code leans heavily on module-level dicts (`SESSIONS_DB`,
`CRYPTO_SESSIONS`, `INSTALLED_APPS`, `AUTH_TOKENS`, `OIDC_STATES`,
`PULL_PROGRESS`, ...) guarded by `asyncio.Lock`. In Rust, consolidate these
into a single `AppState` struct held in `axum`'s state, with
`Arc<tokio::sync::RwLock<HashMap<...>>>` fields. This is a real refactor but
also a cleanup win. The routers currently reach into `api.*` at call time
(see the router docstrings). In Rust you'll pass `&State` explicitly, which
is cleaner.

### 3.6 Database
13 tables, mostly `key + JSON`. **`sqlx`** is the best fit: async, supports
both SQLite and Postgres, compile-time-checked queries, and `serde_json::Value`
for the JSON columns. Port the `TABLE_ORDER`/`TABLE_MODELS` restore logic and
the idempotent `migrate_db()` column checks. Low risk.

## 4. Suggested crate stack

All of the following were verified on crates.io (name, latest version,
activity) as of this evaluation. The ones marked ★ are the ones that close
the gaps I flagged in §3.

```
axum            # HTTP + WS server (replaces FastAPI/uvicorn)
tokio           # async runtime
hyper           # low-level client for the streaming proxy
★ axum-reverse-proxy  # v2.2.0, HTTP session-proxy leg (header rewrite, streaming)
★ tokio-tungstenite   # v0.30, upstream WebSocket leg of the session proxy
★ bollard           # v0.21.1, async Docker Engine API (Unix socket); ~17M recent downloads
sqlx            # async SQLite + Postgres
serde / serde_json# (de)serialization (replaces Pydantic)
ring            # RSA-PSS/OAEP, AES-GCM
argon2          # password hashing
★ openid            # v0.25.0, async OIDC + Discovery client (replaces most of _oidc_*)
★ jwks / jwk-simple # JWKS parsing + key building (replaces the manual JWK→PEM helper)
jsonwebtoken    # remaining id_token claim checks
reqwest         # app-store fetches, health checks, OIDC transport
tar + flate2    # backup/restore
x509-cert       # Let's Encrypt cert parsing
tracing         # logging
uuid, chrono    # ids, timestamps
```

### 4.1 Crate research findings (crates.io, verified)

| Gap flagged in §3 | Crate(s) that fill it | Status |
|---|---|---|
| OIDC / SSO ("no dominant crate") | **`openid`** (v0.25.0, 4.5M dl, active 2026-10); `axum-oidc-client`, `axum-oidc-layer` | **Gap largely closed**, mature and maintained |
| JWKS / id_token verify | **`jwks`** (v0.5.3), **`jwk-simple`** (v0.5.0), `jwks-cache` | Closed: parse + key build + cache |
| Session reverse proxy (HTTP) | **`axum-reverse-proxy`** (v2.2.0, active) | Mostly closed; still need raw-body + cookie logic |
| Session reverse proxy (WS) | **`tokio-tungstenite`** (v0.30, active) | Closed; canonical choice |
| Docker (GPU/DRI3/group_add) | **`bollard`** (v0.21.1, ~17M recent dl, active 2026-08) | Closed; the clear winner |

**Net effect:** the two areas I originally rated as Rust's highest friction
(OIDC and the proxy) both have credible, maintained crates. The residual risk
shifts from "does a crate exist?" to "does the crate's API fit Vreckan's
specific requirements?" In particular, the **raw, still content-encoded
response body** in the session proxy (most reverse-proxy crates decompress for
you) and mapping Vreckan's DB-editable OIDC settings onto `openid`'s config.
That is integration work, not green-field work.

## 5. Migration order (lowest-risk first)

1. **Skeleton:** `axum` app, TLS termination (port `run_https.py`), `AppState`,
   `tracing`, static-file serving of the existing web client.
2. **DB layer:** `sqlx` schema + the 13 tables + `migrate_db` + backup dump.
3. **Settings + RBAC + user_manager:** env/DB precedence, argon2, permissions.
4. **Auth:** password login + opaque tokens, then OIDC.
5. **E2EE:** handshake + `EncryptedRoute` analog.
6. **Catalog + launch + Docker provider** (pull/run/GPU/health).
7. **Session proxy** (HTTP then WebSocket), the largest single piece of work.
8. **Files / uploads / shares / homedirs / pinned.**
9. **Collaboration rooms** (WS fan-out + downstream token broadcast).
10. **Admin endpoints** (bulk of the ~100 routes).
11. **Let's Encrypt + backup/restore.**
12. **Port the 22-file pytest suite** to `tokio`-based integration tests.

## 6. Risks & mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| Session proxy edge cases (raw body, subprotocol, headers) | High | Port the Python comments verbatim; write WS integration tests against a mock upstream |
| Docker GPU/device-request specifics | Medium | Read the Engine API spec; test with the real `docker-compose.yml` (privileged, `/dev/dri`, group_add) |
| OIDC integration (mapping Vreckan's DB-editable settings onto `openid`) | Medium | Use the `openid` crate (or `axum-oidc-client`); keep the existing JWKS verify logic as a fallback; isolate in one module |
| Global-state refactor | Medium | Centralize in `AppState` early; keep the same in-memory semantics |
| ~100 endpoints = volume | Medium | Mechanical work; `serde` models port 1:1 from `models.py` |
| Borrow-checker + async friction | Medium | Prefer `Arc`/`RwLock` ownership; avoid lifetimes across `.await` where possible |

## 7. Performance for this workload

The server is mostly **I/O-bound**: proxying to per-session containers, Docker
daemon calls, and SQLite/Postgres. On I/O-bound work the language's raw speed
matters less, so the realistic performance differences are in **tail latency,
memory, and the crypto hot path**. On all three, Rust is the more efficient
choice.

- **No GC, so low tail latency and jitter.** The data plane streams
  desktop/video (Selkies) over WebSockets, many small, latency-sensitive
  frames. Rust's ownership model has no stop-the-world pauses, so under
  sustained WS streaming there is no GC-induced jitter.
- **Lower memory footprint.** Ownership plus a static binary typically run at
  a fraction of the RSS of a managed runtime, which matters on a self-hosted
  box that is also running the app containers.
- **Crypto hot path.** E2EE does RSA-PSS sign + RSA-OAEP unwrap per handshake
  and AES-256-GCM per request. `ring` is at least as fast as the alternatives
  and uses AES-NI for GCM.
- **Proxy throughput.** `hyper` + `tokio` stream raw bytes with fewer
  intermediate allocations than a header-map/chunk-based proxy.

**Caveat:** at typical self-hosted scale (a handful of concurrent sessions)
the difference would show up in a load test, not in day-to-day use. The one
component where any real difference would live is the session proxy. If
performance matters, port that first and load-test it against the Python
original.

## 8. Verdict

Rust is a **strong, viable** target, and the crate ecosystem closes more of
the gaps than a first pass suggests. The OIDC/SSO area (originally the biggest
concern) is well served by the mature, actively-maintained `openid` crate plus
`jwks`/`jwk-simple`; the session proxy has `axum-reverse-proxy` (HTTP) and
`tokio-tungstenite` (WS); and Docker is cleanly covered by `bollard`. The
residual risk is **integration, not existence**: the session proxy's raw
content-encoded body handling and Vreckan's DB-editable OIDC settings still
need careful, bespoke work. It is also the more efficient choice for this
workload (no GC, lower memory, faster crypto; see §7). If the team is
comfortable with Rust and wants memory safety, a ~10 MB static binary, and
compile-time-checked SQL, go for it. Sequence the proxy and OIDC-integration
work carefully and port the existing test suite to lock in behavior.
