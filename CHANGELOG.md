# Vreckan — Changelog

Vreckan is a hard fork of [SealSkin 0.3.2](https://github.com/linuxserver/docker-sealskin).
The full fork analysis — what was removed, rewritten, and added relative to the
upstream tree — lives in [`HISTORY.md`](./HISTORY.md). This changelog tracks
development *after* the fork, newest first. The Python-era entries below date
from the squashed git history of the original fork; the Rust-port era (2026-10)
is committed normally in `vreckan`. The changelog is the authoritative
record of what shipped in each release — see
[`docs/release.md`](./docs/release.md) for the conventions.

## 2026-11-07 — v0.11.5: base-image bump + runtime zlib1g refresh — clears the PR scan gate (ANT-932)

- **The build base advanced to the 2026-11-07 8:16 AM MST re-bake**
  (`vreckan-base-build` `sha256:1c676e219b49…` → `sha256:a38d1cd2258e…`;
  `.base-image-digest` and the `Dockerfile` build-stage `FROM` re-pointed
  together, ANT-277). This is the first bake built with the `apt-get upgrade`
  layer added to `vreckan-base-build` (vreckan-workflow PR #47, ANT-932),
  which refreshes the pre-existing `rust:1.99.1-slim` parent packages against
  the Debian trixie security repo on every bake — most importantly `zlib1g`
  `1:1.3.dfsg+really1.3.1-1+b1` → `1:1.3.dfsg+really1.3.1-1+deb13u1`,
  fixing CVE-2026-76844 (path traversal) and CVE-2026-85091 (heap overflow in
  `gz_vacate`), both HIGH. Docker had not yet re-cut the
  `rust:1.99.1-slim` tag with the trixie zlib fix as of 2026-11-07 (the tag
  still sat at `sha256:7eab7ce1…`), so the upgrade layer was the only path to
  the fix without waiting on upstream. Verified in the new image: `zlib1g` at
  `deb13u1`, the pinned Rust toolchain unchanged at 1.99.1, and the bake's
  rustup-component and mold end-to-end smoke checks green.

- **The runtime stage now carries the patched `zlib1g` explicitly.** The
  Trivy step of the `VRECKAN: Scan (Rust)` check scans the *final* image —
  the digest-pinned `debian:trixie-slim` runtime stage — not the
  `vreckan-base-build` build stage, so the base advance alone could not clear
  the gate (run 885 on this branch stayed Scan-red for exactly that reason,
  while PR #167's identical runtime fix went green in run 882). The runtime
  `apt-get install` now lists `zlib1g` explicitly, so every rebuild resolves
  `1:1.3.dfsg+really1.3.1-1+deb13u1` from the trixie-security repo at build
  time. This is the same hunk Bobby landed on PR #167 (commit `b50f693`,
  verified locally with trivy v0.76: 0 HIGH/CRITICAL findings), folded into
  this release so the approved PR passes its own scan gate.

- **Operator-visible effect:** since ~5:05 AM MST on 2026-11-07, the Trivy
  step of the `VRECKAN: Scan (Rust)` check was red on every vreckan PR (and on
  the main-branch CI dispatches) because of those two HIGH `zlib1g`
  advisories, and the `main-protection` ruleset made every open PR
  unmergeable. Both stages now carry the fix — the build base via the
  re-bake, the runtime via the explicit install — so any PR branch synced
  onto this commit rebuilds green and the check goes back to passing.

- No other source changes since v0.11.4 — a base-bump-plus-one-Dockerfile-line
  release in the established pattern (v0.10.1, v0.10.2, v0.10.5, v0.10.7,
  v0.11.1, v0.11.3). Once this merges, the zlib hunk in PR #167 becomes
  redundant and the branch re-cuts to 0.11.6 under the whichever-merges-first
  rule.
## 2026-11-07 — v0.11.5: dependency consolidation — lock 592 → 581, websocket stack compiles once, shared test-support crate

- **The lockfile drops 12 packages and gains 1 (592 → 581)** (ANT-849).
  The dead direct deps `axum-reverse-proxy` and `axum-extra` (zero source
  references) are removed along with the `cookie` 0.18 and
  `tungstenite`/`tokio-tungstenite` 0.28 copies they pulled (ANT-852), the
  0.30 tungstenite pair is gone with the websocket stack collapsed 3→1 onto
  axum's 0.29 (ANT-854), and the duplicate `jsonwebtoken` 10.4.0 copy plus
  the `jwks` crate are removed (ANT-855). The single addition is the new
  `vreckan-test-support` dev crate (ANT-853). Operator-visible effect: the
  websocket stack compiles once instead of three times and one copy of
  jsonwebtoken is gone, so expect the compile-time and binary-size savings
  from those.

- **JWT verification no longer depends on the `jwks` crate** (ANT-855).
  `vreckan-auth` hand-rolls the JWKS fetch on reqwest against jsonwebtoken
  11: RS256 pinned (not header-trusted), per-`kid` fail-closed, same
  aud/exp/iss semantics as before. The crypto-equivalence review (ANT-863)
  passed clean. Note for operators tracking advisories: the v10-era crypto
  family (including `crypto-bigint` 0.5.5) remains in the tree via
  jsonwebtoken 11's `rust_crypto` feature until upstream adopts p256/p384
  0.14, so the RUSTSEC-2026-0387 allowances stand (ANT-546).

- **The test harness de-duplicates**: the five drifted
  `tests/common/mod.rs` copies (~2,900 lines across admin, auth, catalog,
  files, sessions) now share one `vreckan-test-support` dev crate
  (~2,500 duplicated lines removed) (ANT-853). Test behavior is unchanged;
  the full workspace suite is green, and the websocket 0.29 regression
  (ANT-862) passed.

## 2026-11-07 — v0.11.4: base-image bump + bounded memory on the anonymous/proxy paths (audit findings 15 & 18)

- **The build base advanced to the 2026-11-06 10:47 PM MST re-bake** (`vreckan-base-build`
  `sha256:32264d2a0ec2…` → `sha256:1c676e219b49…`; `.base-image-digest` and the
  `Dockerfile` build-stage `FROM` re-pointed together, ANT-277). The re-bake
  moved the upstream `rust` base digest (`sha256:4195a351…` →
  `sha256:7eab7ce1…`) and refreshed the warm crates.io/git dependency cache
  against the current nine-crate lockfile; the pinned Rust toolchain stays at
  1.99.1. Built on gitea-worker02 by the daily re-bake cron, which has been
  healthy since the ANT-841 workflow fix.

- **Memory is now bounded on the anonymous and proxy request paths**
  (ANT-886, audit finding 15): public shares are capped at 1 GiB, share
  downloads stream instead of fully buffering, the app-launch copy runs
  disk-to-disk rather than through process memory, and the Selkies proxy
  streams request bodies. An operator moving large files through the app
  catalog or session proxy no longer has to keep them resident in RAM.

- **`recreate_session` no longer holds the catalog write lock across its
  container work** (audit finding 18): the lock is dropped while the
  recreate path waits on Docker, so one slow container recreate can no
  longer serialize other catalog operations.

- **Loose CORS stays the default, now recorded as accepted risk**
  (ANT-880, audit finding 9): the empty-allowlist behavior is documented as
  a known, accepted trade-off; the allowlist remains optional for operators
  who want strict mode.

- **Docs/site freshness pass** (ANT-870, plus the v0.11.3 sync from
  ANT-886): table counts, app-pages Join/badge and `#groups` anchor,
  api.html sync (CORS payload, logo section, crate layout, 19 permissions),
  site.css contrast, compose-knob env rows, setup current-release →
  v0.11.3, and the pin examples + public-share 1 GiB cap docs.

## 2026-11-06 — v0.11.3: base-image bump + docs/site sync to the v0.11.2 release

- **The build base advanced to the 2026-11-05 re-bake** (`vreckan-base-build`
  `sha256:f33243e8e60c…` → `sha256:32264d2a0ec2…`; `.base-image-digest` and the
  `Dockerfile` build-stage `FROM` re-pointed together, ANT-277). The re-bake
  moved the upstream `rust` base digest (`sha256:9daf2b24…` →
  `sha256:4195a351…`) and refreshed the warm crates.io/git dependency cache
  against the v0.11.2 nine-crate Phase-3 tree; the pinned Rust toolchain stays
  at 1.99.1. It was built from the ANT-841 workflow fix (change-check syntax +
  the workspace-following sparse checkout), so the daily re-bake cron is
  healthy again after failing on that syntax error since 2026-11-03.

- **The runtime stage's `debian:trixie-slim` is now digest-pinned** (it had
  been a bare tag since the multi-stage Dockerfile landed in ANT-3). The pin
  takes the 2026-11-04 manifest (`sha256:a72b4433de1c…`, `apt` 3.0.4), which
  clears **CVE-2026-109518** (HIGH, on-path attack via `apt`/`libapt-pkg`,
  fixed in 3.0.4): the 2026-11-05 nightly had still resolved the 2026-10-27
  manifest (`apt` 3.0.3) before Debian flipped the tag, and the finding held
  that nightly's push. The pin makes the runtime base reproducible going
  forward, matching the digest convention the site image base has followed
  since ANT-317/ANT-358.

- **No code changes since v0.11.2** — this release carries the docs/site
  sync batch only:
  - The site ops page gained the previously missing
    "Pull concurrency & rate-limit retries" section (ops §3.4, the ANT-668
    drift; ANT-834).
  - Docs and site were synced to v0.11.2: the nine-crate workspace layout,
    the aws-lc-rs crypto unification, and stale ops fixes (ANT-832).
  - `port-status.md` gained the v0.11.2 (Phase 3) status entry with the
    post-split crate paths (ANT-831).

## 2026-11-04 — v0.11.2: Phase 3 — apps-cluster dissolution, crypto unification, per-crate test suite (ANT-662 P3)

The single consolidated Phase 3 PR, per the board's single-PR direction
(ANT-807, following the single-PR preference recorded for v0.10.0 at
ANT-650 rev 5): the three board-approved track branches are folded onto
`main` in the order **812 → 811 → 810** with each track's commit
attribution preserved (per-item commits, as the v0.10.0 fold), followed
by the one-line pin from PR #137 (ANT-820). The workspace ends the phase
as nine crates; the crate DAG is acyclic:
`core <- collab <- sessions <- catalog <- files <- admin <- vreckan-bin`.

- **`vreckan-apps` is dissolved into four domain crates** (ANT-812):
  `vreckan-collab` (collaboration rooms: token tables, WebSocket fan-out),
  `vreckan-sessions` (session shares + Selkies session proxy),
  `vreckan-catalog` (app catalog/store, image pulls, launches + the
  Docker provider — their mutual type usage keeps them in one crate), and
  `vreckan-files` (uploads, storage, public shares); `backup_manager`
  moves to `vreckan-admin` (the one module fanning out to
  catalog+files+sessions). The cycle-breaking moves into `vreckan-core`
  are plain data/functions (`shared::random_token` / `random_uuid`,
  `state::save_sessions` / `load_sessions`,
  `static_files::render_password_page`), so the no-core→domain-edge
  property holds. The public API is preserved: the `vreckan` facade
  re-exports every module under its original name, and the moved helpers
  re-export at their old paths, so downstream `Cargo.toml` and
  `use vreckan::…` are unaffected.

- **The crypto provider stack unifies on aws-lc-rs** (ANT-811): `rustls`
  moves from `features = ["ring"]` to `["aws_lc_rs"]` (the old spec plus
  rustls 0.23's default features compiled both provider backends into
  one rustls unit); `rcgen` is pinned to the same spec as the acmex fork
  so one rcgen unit with one provider is built. The dead direct deps —
  the root package's `ring` and `openid` — had no references (OIDC is
  hand-rolled; `jwks` does not depend on the `openid` crate), so the
  whole `openid → biscuit → ring` chain, and with it reqwest's
  `__native-tls`/openssl path, leaves the tree; `cargo tree -i ring` is
  now empty for the host target. The acmex fork drops `fips` from its
  aws-lc-rs dep (new tag `vreckan-2026-11-03`, commit 771333e), removing
  the second full C build of the aws-lc source (aws-lc-fips-sys, ~64.7 s
  in the ANT-766 cold measurement) with no runtime path exercising FIPS
  mode; `deny.toml`'s OpenSSL license allow entry drops with it. No
  behavior change for shipped TLS/ACME — the provider shim already
  preferred aws-lc-rs.

- **The test suite splits into per-crate `tests/` trees** (ANT-810):
  each integration test lives in, and dev-depends only on, the crate(s)
  it exercises, instead of all 30 binaries linking the whole program
  through the root facade. `vreckan-admin` keeps the 16 full-router
  tests plus the new `test_admin_gate` (split out of `test_auth` so the
  rest of the auth suite stops linking admin); the 13 former
  `vreckan-apps` tests are re-homed to the crates whose routes they
  exercise — `vreckan-catalog` (7), `vreckan-sessions` (3),
  `vreckan-files` (3) — each with a per-crate `tests/common` harness
  mirroring the app router minus the `/api/admin` nest in production
  nest/merge order, so route precedence is unchanged for every route a
  test hits and the admin/root-bin link-closure exclusion the split was
  about is preserved; the single auth test moves to `vreckan-auth` with
  a minimal harness. The security-headers/CORS module moves from
  `vreckan-admin` to `vreckan-core` (it only uses core types) so the
  per-crate harnesses apply the same middleware without linking admin
  (admin re-exports it; the facade keeps the pre-split path).
  Dev-dependencies are trimmed per crate, the root package (no test
  targets left) drops its dev-deps, and the Dockerfile test stage + the
  Gitea CI test job move from `cargo test` to `cargo test --workspace`
  (a plain `cargo test` would now select only the test-less root
  package). The test set is equivalent: 30 files moved, 1 test re-homed,
  no assertions changed. The re-homed sessions tests additionally link
  `vreckan-catalog` as a **dev-dependency** (the shared harness composes
  the catalog routes the session flows hit — join, my-sessions — and
  persists session state through `catalog::save_sessions`); the
  production DAG edge stays catalog→sessions, so no runtime impact.

- **`axum-reverse-proxy` pinned 2.2 → 2.4** (PR #137 / ANT-820): a
  one-line pin in the root `Cargo.toml` closing the regression the
  Phase 2 split left on `main` against the v0.10.6 release; the
  lockfile already resolved 2.4.0, so the pin removes the drift rather
  than moving the graph.

- **Measurement marker (no code)** (ANT-808): the pre-split tree's
  typical-change build measured 46m0.6s under ~7× host-load contention —
  recorded here for the report; the post-Phase-3 numbers under the same
  methodology are in the consolidated ANT-821 report and the bullet
  below.

- **Consolidated Phase 3 measurement** (ANT-821): BEFORE
  (`main` @ `a24a1df`, v0.11.1) vs AFTER (this release) on paperclip01,
  `vreckan-base-build:latest`, `CARGO_BUILD_JOBS=8`, dedicated empty
  sccache + fresh `target/` per tree — full cold pipeline
  **2640s → 2505s (−135s, −5.1%)**: release build 437s → 453s, cold
  test build 1775s → 1663s (−112s, −6.3%), full suite 428s → 389s
  (−39s, −9.1%). Suite results are identical — **614 passed / 0
  failed** on both trees (43 test targets before, 50 after the
  per-crate split). Both trees ran back-to-back on the same host under
  the same protocol (load average ~2–8 across the pair window), so the
  comparison is like-for-like. sccache: the BEFORE tree recorded zero
  cache activity (the same anomaly seen in the first measurement pair),
  while the AFTER tree served 263 of 1426 executed compile requests
  (18.6%) from cache — 50% for C/C++ units (within-run reuse, e.g.
  aws-lc-sys) and 0.22% for Rust.

- **Dependency footnote** (folded from the pending note at this cut,
  ANT-826): the deliberate `ctutils 0.4.5` pin (ANT-513, the
  RUSTSEC-2026-0385 fix) was superseded by the v0.11.0 `rsa` rc.20
  subgraph float, which moved `ctutils` to 0.4.8 — the ANT-818 audit
  confirms 0.4.8 carries the fix, so the security baseline is unchanged.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.11.1 → 0.11.2 via
`scripts/bump-version.sh patch` (with every workspace member's
`Cargo.lock` entry re-pointed in the same run).

## 2026-11-04 — v0.11.1: base-image bump + workspace-crate lockfile-sync fix (ANT-805)

- **The build base advanced to the latest `vreckan-base-build`** (the
  2026-11-03 8:32 AM MST re-bake, tagged `20261103`): `.base-image-digest`
  and the `Dockerfile` build-stage pin move together (ANT-277) from
  `sha256:001a81a8…` (the 2026-11-02 6:22 PM MST re-bake, which tracked
  pre-split v0.10.6-era `main` at `bb98553`) to
  `sha256:f33243e8…`, which tracks vreckan `main` at `aeb2074` — the
  six-crate Candidate C workspace with the ANT-791 `rust-toolchain.toml`
  pin (the parent of the v0.11.0 tag). The bake keeps the same
  `rust:1.99-slim` `sha256:9daf2b24…` base (Rust 1.99.1), the sccache
  wrapper, and the previous bake's `mold`-as-default-linker
  `RUSTFLAGS`; what moved is the warm crates.io/git dependency cache,
  which now matches the post-split six-crate `Cargo.lock` — local and CI
  builds resolve the current lockfile straight from the warm cache
  instead of fetching the split-era deltas. For operators: the old bake
  is no longer tagged in the Gitea registry — the `cleanup_packages` cron
  reclaimed its blobs after the 11-03 re-bake re-tagged `latest` — so
  `Dockerfile` builds on the previous pin fail at the base-image pull
  until this re-point lands.

- **`scripts/bump-version.sh` now re-points all six workspace crates'
  `Cargo.lock` entries** (ANT-805): after the Candidate C split (v0.11.0)
  every workspace member carries its own version entry in `Cargo.lock`,
  and each bump drifted the five non-root members until something else
  forced a lockfile rewrite. The script now parses the `[workspace]`
  members list, reads each member's package name from its own manifest,
  and re-points every member's `Cargo.lock` entry alongside `VERSION` and
  the root `Cargo.toml`, with a read-back self-verify on each. No
  user-visible behavior change — versioning hygiene so the three files
  never drift again (the 0.8.5/ANT-450 and post-split/ANT-805 drifts were
  the counter-examples).

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.11.0 → 0.11.1 via
`scripts/bump-version.sh patch` (with the six workspace crates'
`Cargo.lock` entries re-pointed in the same run).

## 2026-11-03 — v0.11.0: Candidate C Cargo workspace split (ANT-662 P2)

- **The monolith is now a Cargo workspace of six crates** (Candidate C,
  ANT-662 Phase 2 / ANT-770): the former single lib+bin is split into five
  domain crates — `vreckan-core`, `vreckan-acme`, `vreckan-auth`,
  `vreckan-apps`, `vreckan-admin` — plus a thin `vreckan` package that holds
  the binary and re-exports every former top-level module as the public lib
  facade. Downstream dependents' `Cargo.toml` and `use vreckan::...` paths are
  unaffected. The bounded 7-type data-model extraction into `vreckan-core`
  (ANT-773) landed with the split.

- **Version bump 0.10.7 → 0.11.0** (ANT-775): a minor bump shipped with the
  split, per the board ask that the version bump ships alongside it (ANT-662
  P2). All six crates inherit the single `workspace.package.version` via
  `version = { workspace = true }`, so `VERSION`, the root `Cargo.toml`, and
  every crate's `Cargo.lock` entry move together to 0.11.0.

- **`rsa` 0.10.0-rc.19 → 0.10.0-rc.20** (Renovate, PR #129 / ANT-801): the
  held RustCrypto `rsa` pin advanced to rc.20 and lands immediately before the
  bump so the rc.20 pin ships in v0.11.0. rc.20 makes `rsa`'s `sha2`
  dependency required, so the manifest's `features = ["sha2"]` is dropped
  (no code change — every `rsa::…` call site uses the unchanged root-level
  re-exports). The `cargo update -p rsa` subgraph floats to current:
  `crypto-bigint 0.7.6 → 0.7.10`, `ctutils 0.4.5 → 0.4.8`, `pkcs1
  0.8.0-rc.5 → 0.8.0` (no new packages enter the tree).

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.10.7 → 0.11.0 via
`scripts/bump-version.sh minor` (with the five non-root crate `Cargo.lock`
entries re-pointed alongside the root).

## 2026-11-03 — v0.10.7: base-image bump + mold as the default Rust linker (ANT-772)

- **The build base advanced to the latest `vreckan-base-build`** (the
  2026-11-02 6:22 PM MST re-bake): `.base-image-digest` and the `Dockerfile`
  build-stage pin move together (ANT-277) from `sha256:ebc259cd…` (the
  2026-11-01 10:30 PM MST re-bake, which tracked the v0.10.4-era commit
  `df82564` and was built on `rust:1.99-slim` `sha256:b8e062b1…`) to
  `sha256:001a81a8…`, which tracks vreckan `main` at the current HEAD
  `bb98553` (v0.10.6) and is built on `rust:1.99-slim`
  `sha256:9daf2b24…` — the upstream `docker-rust` base digest moved between
  the two bakes.

- **The re-bake adds `mold`, wired as the default Rust linker** (ANT-772,
  ANT-662 P2): the base sets
  `RUSTFLAGS="-Clinker=cc -Clink-arg=-fuse-ld=mold"`, so every link — the
  release binary and all of the integration-test binaries — now runs through
  `mold` instead of `rust-lld`, with no consumer-side config (a consumer
  that wants the default linker back overrides `RUSTFLAGS` itself). The
  `-sys` C builds are unaffected. For operators: faster release builds and
  test-binary linking through the base image's toolchain, including the
  vreckan image's own build stage.

- **The warm crates.io/git dependency cache was refreshed against the
  current `Cargo.lock`**: the prior bake predated the v0.10.6 dependency
  moves (`axum-reverse-proxy 2.3 → 2.4`, plus the new `tokio-util` /
  `futures-util` edges), so local and CI builds now resolve the current
  lockfile straight from the warm cache.

- No other user- or operator-visible changes since v0.10.6 — this cut is
  the base-image bump itself.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.10.6 → 0.10.7 via
`scripts/bump-version.sh patch`.

## 2026-11-02 — v0.10.6: axum-reverse-proxy 2.3 → 2.4 (ANT-756)

- **The `axum-reverse-proxy` dependency advanced 2.3 → 2.4** (Renovate PR #126,
  board-approved via [ANT-752](/ANT/issues/ANT-752)). Release 2.4 adds a
  `TunnelGuard` request extension — it retains owned permits and metrics
  through response bodies or actual upgraded-tunnel teardown, with
  single-consumption clone semantics — and an optional `TunnelCancellation`
  hook. The no-TLS default connector is also fixed to reject HTTPS targets
  instead of silently sending plaintext. The dependency graph gains two edges
  (`tokio-util` for `axum-reverse-proxy`, `futures-util` for `h2`); no new
  packages and no other version moves. Nothing user-visible changes beyond the
  dependency update.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.10.5 → 0.10.6 via
`scripts/bump-version.sh patch`.

## 2026-11-02 — v0.10.5: base-image bump + weekly docs freshness pass (ANT-745; ANT-708)

- **The build base advanced to the latest `vreckan-base-build`** (the
  2026-11-02 re-bake): `.base-image-digest` and the `Dockerfile` build-stage
  pin move together (ANT-277) from `sha256:8ef56842…` (the 2026-11-01 re-bake,
  which tracked the v0.10.2-era commit `043586f`) to
  `sha256:ebc259cd…`, which tracks `main` at the v0.10.4-era tree
  (`df82564`, the ANT-708 docs pass). The re-bake refreshes the warm
  crates.io/git dependency cache against the current `Cargo.lock` — the prior
  bake predated the v0.10.4 dependency moves (`axum-reverse-proxy 2.2 → 2.3`,
  transitive `getrandom 0.3.4 → 0.4.3`), so local and CI builds now resolve the
  current lockfile straight from the warm cache. The upstream `docker-rust`
  base digest (`b8e062b1…`) and the Rust toolchain are unchanged, so nothing
  user-visible changes with the container bump itself.

- **Weekly docs freshness pass (ANT-708, PR #121).** Docs-only — no runtime
  behavior changes. The site setup/ops pages and `docs/ops.md` now point at
  the current release (v0.10.1 → v0.10.4), the site ops page gains the CORS
  and HTTP→HTTPS redirect sections that `docs/ops.md` §2.1/§5 describe, and
  `.env.example`/README gain the `VRECKAN_*` settings introduced since the
  file was last touched — `VRECKAN_CORS_MODE` / `VRECKAN_CORS_ALLOWED_ORIGINS`
  (v0.10.0), `VRECKAN_TLS_HTTP_REDIRECT_ENABLED` (v0.10.0),
  `VRECKAN_ALLOW_DEFAULT_DB_CREDS`, and
  `VRECKAN_REGISTRY_PULL_CONCURRENCY` — with the README configuration table
  updated to match. The setup page's raw-markdown-in-HTML bold rendering is
  fixed (kramdown skips block HTML), and the API docs' scale line is
  refreshed on both the site and `docs/api.md`.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.10.4 → 0.10.5 via
`scripts/bump-version.sh patch`. `.base-image-digest`/`Dockerfile` moved
together to the new base digest.

## 2026-11-01 — v0.10.4: axum-reverse-proxy 2.2 → 2.3 (ANT-741)

- **The `axum-reverse-proxy` dependency advanced 2.2 → 2.3** (Renovate PR #124,
  board-approved via [ANT-735](/ANT/issues/ANT-735)). Release 2.3 adds the
  `proxy_request_on_stream` API — one HTTP/1.1 request over a caller-authorized
  stream, with streaming bodies and WebSocket upgrades — and WebSockets now use
  the configured hyper client/connector and its TLS settings. The transitive
  dep set moves with it: `getrandom` 0.3.4 → 0.4.3. Nothing user-visible
  changes beyond the dependency update.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.10.3 → 0.10.4 via
`scripts/bump-version.sh patch`.

## 2026-11-01 — v0.10.3: base-image bump, no source changes since v0.10.2 (ANT-733)

- **The build base advanced to the latest `vreckan-base-build`** (the
  2026-11-01 re-bake): `.base-image-digest` and the `Dockerfile` build-stage
  pin move together (ANT-277) from `sha256:40e2ecba…` to `sha256:8ef56842…`,
  re-baking the warm crates.io/git dependency cache against the current
  v0.10.2 tree. The re-bake tracked the v0.10.2 commit (`043586f`) where the
  prior bake tracked the v0.10.1 tree; the dependency set itself is unchanged
  (`Cargo.lock` is byte-identical apart from the vreckan version line). The
  Rust toolchain and the upstream `docker-rust` base digest are unchanged
  (`b8e062b1…`), so nothing user-visible changes.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.10.2 → 0.10.3 via
`scripts/bump-version.sh patch`. `.base-image-digest`/`Dockerfile` moved
together to the new base digest.

## 2026-10-31 — v0.10.2: base-image bump, no source changes since v0.10.1 (ANT-721)

- **The build base advanced to the latest `vreckan-base-build`** (the
  2026-10-31 re-bake): `.base-image-digest` and the `Dockerfile` build-stage
  pin move together (ANT-277) from `sha256:c58ba357…` to `sha256:40e2ecba…`,
  refreshing the baked dependency cache against the current `Cargo.lock`.
  The Rust toolchain and upstream base are unchanged — nothing user-visible
  changes.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.10.1 → 0.10.2 via
`scripts/bump-version.sh patch`.

## 2026-10-30 — v0.10.1: base-image bump, no source changes since v0.10.0 (ANT-703)

Nightly base-bump cut. `main` has no commits past the `v0.10.0` tag (`git log
v0.10.0..HEAD` is empty), so this is a build-base-only patch release: the
shipped image is `v0.10.0`'s source compiled on the refreshed build base.

- **The build base advanced to the latest `vreckan-base-build` image.** The
  `Dockerfile` build-stage pin and `.base-image-digest` move together (ANT-277)
  from `sha256:97c4f4ce…` (the 2026-10-29 re-bake) to `sha256:c58ba357…` (the
  2026-10-30 re-bake). The re-bake fired because vreckan's `main` moved past the
  commit the current base was baked against: the prior bake tracked the
  `571b57d` base-pin commit, while the new bake tracks the `v0.10.0` tree
  (`9877822`), so the warm crates.io/git dependency cache is now baked against
  the current `Cargo.lock` — which the v0.10.0 batch extended with
  `hyper-util` and `rustls-pki-types`. The Rust toolchain inside is unchanged
  (1.99.1) and the upstream `docker-rust` base digest is unchanged
  (`b8e062b1…`), so nothing user-visible changes with this container bump.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.10.0 → 0.10.1 via
`scripts/bump-version.sh patch`. `.base-image-digest`/`Dockerfile` moved
together to the new base digest.

## 2026-10-29 — v0.10.0: the external-feedback batch — ten fixes plus the 20261029 build-base bump (ANT-650)

v0.10.0 ships the ten-item external feedback batch (ANT-650) together with
the build-base re-bake (ANT-673). All items' work landed as a single PR
against `main`; item 2 (loose CORS) and item 9 (autoupdate card) had already
merged directly to `main` mid-batch, so the release diff dedupes against
them. Before merge, QA (ANT-685) ran the full suite — 560 passed / 0 failed
(12 daemon-dependent tests self-skip in the docker-less build image) — plus
a real-Docker e2e pass over every behavior-changing item: all PASS.

- **Plain HTTP to the TLS port now redirects to HTTPS (ANT-652).** The
  container runs one listener — TLS on 443 — so a plain-HTTP connection to
  that port used to die in the TLS handshake ("wrong version number") before
  any request was parsed. The server now peeks the first byte of each new
  connection: a TLS ClientHello is served as before, anything else is
  answered `301 https://{Host}{uri}` with the external port preserved and
  HSTS attached — the same one-port technique Caddy and nginx use. New
  `tls_http_redirect_enabled` setting (admin UI → Settings, or
  `VRECKAN_TLS_HTTP_REDIRECT_ENABLED`), default **on**; reverse-proxy
  deployments that terminate TLS themselves should flip it off. Bare
  `http://host` on port 80 is still refused — a separate port-80 listener
  remains a documented future option.
- **Loose CORS is now the default, so a page on a different origin can use
  your login (ANT-653).** Before this release the server sent no
  `Access-Control-Allow-Origin` headers at all and the login cookie was
  `SameSite=Lax` — both right for a same-origin client, but the moment you
  reach the same server under a different name (the LAN IP instead of the
  hostname, a second DNS name, or a different port), the browser treats it
  as a different site, drops the cookie, and login "doesn't work". Now the
  `cors_mode` setting (admin UI → Settings → CORS, or
  `VRECKAN_CORS_MODE`) selects the behaviour: `loose` (the new default)
  echoes the requesting `Origin` with `Access-Control-Allow-Credentials:
  true`, answers OPTIONS preflights directly (methods plus headers including
  `X-Session-ID` and `Content-Type`), issues the login cookie
  `SameSite=None; Secure`, and relaxes the three cross-origin-relevant
  security headers (the CSP's `connect-src`, `Cross-Origin-Opener-Policy`,
  `Cross-Origin-Resource-Policy`). `strict` reproduces the pre-v0.10.0
  header set exactly. The effective mode is re-read on every request, so a
  change saved on the admin page applies to the next request — no restart.
- **Optional origin allowlist (`VRECKAN_CORS_ALLOWED_ORIGINS`).** CORS with
  credentials is the classic footgun — with fully-open loose mode any site
  open in the admin's browser can make credentialed calls. The trade-off is
  bounded by the E2EE request body, which remains the CSRF gate on every
  state-mutating route (the same gate that protects `join_my_session`), and
  by this optional comma-separated origin list: in loose mode only listed
  origins (compared case-insensitively) get the CORS headers; everything
  else is treated as in strict mode. Empty (the default) accepts every
  origin.
- **Admin UI: CORS card on the Settings page.** The effective mode and
  allowlist are shown with the usual source badges (database / environment
  / default) and are editable like the rest of the page.
- **Docs.** `docs/ops.md` gains a CORS section (§2.1) with the deployment
  trade-off, and the README's configuration table documents the two new
  environment variables.
- **Launch and kill feel responsive again (ANT-654).** The launch modal
  closes as soon as the launch is fired instead of holding the window open
  until the container passes its health check; a "Starting…" status and a
  waiting screen in the session tab cover the wait, and the tab flips to
  the app the moment it is ready. Kill responds immediately with a
  "Stopping…" state, and the row drops only after the container has
  settled — no more dead-looking kill button.
- **Blocked session popups get a fallback (ANT-661).** Browsers allow
  programmatic `window.open` only while the click's user activation is still
  live, and a launch taking an image pull or up to 60 s of readiness often
  blew past it — the source of the intermittent "launch with a custom name
  doesn't open a tab" reports. The session tab is now opened at click time,
  and if the popup blocker still swallows it, launch, Join, and
  Open-in-app show a "session started — click to open" toast with a
  highlighted session row instead of failing silently.
- **The install modal stops offering a pointless template dropdown
  (ANT-665).** Templates are now tagged with their provenance (built-in
  default vs custom) at load time; when you have no custom templates the
  dropdown disappears and the default template is sent automatically —
  which also fixes a latent lowercase-`default` client literal that could
  never match the seeded `Default` row.
- **App-download progress no longer jumps 0% → 100% (ANT-656).** The pull
  progress parser looked for fields the Docker daemon never emits, so the
  percentage stayed `null` for the whole pull and the UI snapped to 100% on
  completion. It now reads the per-layer counters from the daemon's
  `progressDetail`, so the install modal and table rows advance smoothly
  through a real pull; the phase text remains the primary indicator, and
  daemons that emit no counters get an indeterminate shimmer.
- **App-store browsing is icon-led (ANT-657).** Available-apps rows and the
  Installed Apps table now lead with each app's 32 px icon — the catalog's
  `logo` field, with the existing fallback tile for third-party stores that
  omit the field or 404 the remote image.
- **Image pulls survive transient registry errors (ANT-668).** A failed
  pull — for example LinuxServer.io's per-IP rate limit surfacing as
  `toomanyrequests` — used to hard-fail on the first stream error; it is now
  retried with backoff and surfaced as a clean one-line error. A global
  semaphore also caps concurrent pulls at one, so the auto-update tick stops
  fanning out a pull per installed app and spiking the registry's per-IP
  budget. (The registry mirror-cache phase of the design is deferred; a
  documented no-op seam is in place.)
- **The autoupdate card shows the right time (ANT-659).** The card fed unix
  seconds where `Date()` expects milliseconds, so "Last checked" rendered as
  a 1970 date and "Next check" stuck on "0 min" — the checker itself was
  running fine. Both values now render the actual times.
- **The Certificates card knows about the built-in self-signed cert
  (ANT-660).** With ACME off (the default), the card used to say "No
  certificate found" even though the server was serving its built-in
  self-signed certificate. It now reports the certificate as
  `self-signed` with its expiry; the status is display-only, so the ACME
  renewal logic is untouched.
- **The build base advanced to the latest `vreckan-base-build` bake.** The
  `Dockerfile` build-stage pin and `.base-image-digest` move together
  (ANT-277) from `sha256:6f0055d3…` (the 2026-10-28 re-bake, since reclaimed
  by registry GC) to `sha256:97c4f4ce…` (tag `20261029`, the only live
  bake). The re-bake tracks vreckan `main` at the ANT-669 pin fix
  (`571b57d`) and vreckan-workflow at `b8d20634` (re-landed test-image cap +
  BuildKit cache size cap in the `build-test-rust` cleanup job). The Rust
  toolchain inside is unchanged (1.99), so nothing user-visible changes with
  the container bump itself. This supersedes the in-flight PR #114 pin
  sync, whose target bake had already been GC'd.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.9.4 → 0.10.0 via
`scripts/bump-version.sh minor`. `.base-image-digest`/`Dockerfile` moved
together to `sha256:97c4f4ce…`.

## 2026-10-28 — v0.9.4: base-image bump, no source changes since v0.9.3 (ANT-638)

Nightly base-bump cut. `main` has no commits past the `v0.9.3` tag (`git log
v0.9.3..HEAD` is empty), so this is a build-base-only patch release: the
shipped image is `v0.9.3`'s source compiled on the refreshed build base.

- **The build base advanced to the latest `vreckan-base-build` image.** The
  `Dockerfile` build-stage pin and `.base-image-digest` move together (ANT-277)
  from `sha256:7b2cc27f…` (the 2026-10-27 re-bake) to `sha256:6f0055d3…` (the
  2026-10-28 re-bake). The re-bake fired because vreckan's `main` moved past
  the commit the current base was baked against: the new bake tracks the
  `v0.9.3` tree (`7a54f73`), so the warm crates.io/git dependency cache is now
  baked against the current `Cargo.lock`. The Rust toolchain inside is
  unchanged (rustc/cargo verified at 1.99.1 by running the new base image), so
  nothing user-visible changes with this container bump.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.9.3 → 0.9.4 via
`scripts/bump-version.sh patch`. `.base-image-digest`/`Dockerfile` moved
together to the new base digest.

## 2026-10-29 — v0.9.3: "URL to open" is now optional for browser apps, and the locale catalogs are back in sync (ANT-626, ANT-628)

- **"URL to open" is now optional for browser apps
  (ANT-626).** Launching Firefox, Chrome, or Brave with a blank URL opens
  the app as-is (its default page) instead of showing a "required" error. A
  no-URL launch leaves `$VRECKAN_URL` unset (not empty) in the session env —
  launch-time URL semantics apply only when the variable is present, which is
  what meta-app (Laboratory) autostart scripts key off.

- **i18n synchronized across all 17 non-English locales
  (ANT-628/T1).** 33 keys missing from every non-English locale since v0.9.2
  were added; all 18 locale files now carry an identical key set.

- **Process note for future UI work:** new strings must be added to **all**
  files under `server/static/i18n/`, not just `en.json` — the drift this
  release fixes accumulated because strings landed in English only.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.9.2 → 0.9.3 via
`scripts/bump-version.sh patch`.

## 2026-10-27 — v0.9.2: "Join" offered only where you're invited — on rows, not tiles, with a slimmer join confirmation (ANT-618)

- **"Join" is now offered only where it is actually usable
  (ANT-618).** On the Apps page, the Join action (ANT-440 — a second user
  joining a running session) is cleaned up in three places. Your own
  Active Sessions rows no longer render a Join next to "Open"
  (`!mine && isJoinable(s)` in `server/static/js/app/sessions.js`);
  other users' session rows show Join when — and only when — the caller
  has been invited (the owner's express grant, ANT-472), which is also
  what makes the session appear in the caller's Active Sessions list at
  all. The app tiles drop their Join button entirely: an invited session
  is joined from its Active Sessions row, not from the card
  (`server/static/js/app.js`). And the pre-join confirmation modal
  (`sessions.joinConfirmMsg` in `server/static/i18n/en.json`) now states
  only the participant role — "You'll join "{name}" as a participant with
  input control." — dropping the WebSockets/selkies transport notes
  (still tracked in ANT-443) and internal ticket references from
  user-facing text.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.9.1 → 0.9.2 via
`scripts/bump-version.sh patch`.

## 2026-10-27 — v0.9.1: base-image bump + last Windows-written files normalized (PR #95, ANT-589)

- **The underlying Rust build container was bumped** to the
  2026-10-27 `vreckan-base-build` image
  (`sha256:2734691e…` → `sha256:7b2cc27f…`). The Rust toolchain inside is
  unchanged (still 1.99.1), so nothing user-visible changed with this
  container bump.

- **The last of the Windows-written files were converted to LF line
  endings (PR #95):** `LICENSE`, `PRIVACY.md`, `README.md`, and the four
  `site/assets/img` SVGs. The same PR brought `HISTORY.md` up to date
  (ANT-589).

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.9.0 → 0.9.1 via
`scripts/bump-version.sh patch`.

## 2026-10-26 — v0.9.0: collaboration-room Font Awesome icons vendored (ANT-570, PR #94)

First source change on `main` since `v0.8.9` (v0.8.10 was a base-bump-only
cut). The board directed the version move 0.8.10 → 0.9.0 alongside the fix
below.

- **The collaboration room no longer loads Font Awesome from cdnjs
  (ANT-570).** `server/static/collaboration/room.html` linked
  `https://cdnjs.cloudflare.com/ajax/libs/font-awesome/6.4.0/css/all.min.css`,
  which neither CSP in play for the room allows — the global `style-src
  'self'` and the room's strict nonce CSP (`'nonce-…'` authorizes inline
  styles only) — so all 19 room icons (mute, reload stream, mic, video,
  settings, invite, chat, send) rendered blank. The fix vendors
  Font Awesome Free 6.4.0 into
  `server/static/collaboration/font-awesome/` — `css/all.min.css` plus the
  single webfont the room actually loads, `webfonts/fa-solid-900.woff2`
  (every room icon uses the `fas` family; the `far`/`fab` `@font-face`
  rules in the CSS never trigger, so their webfonts are not needed) — with
  the CC BY 4.0 / OFL 1.1 / MIT attribution in `NOTICE.md`. The `<link>`
  now points at the absolute path
  `/collaboration/font-awesome/css/all.min.css`, so both the rendered
  `/room/{session_id}` page and the static `/collaboration/room.html`
  entry resolve the same local file under `style-src 'self'` with no CSP
  change; air-gapped deployments where cdnjs is unreachable gain the icons
  as well. A regression test (`test_room_font_awesome_vendored` in
  `tests/test_security.rs`) pins the local link and asserts the CSS
  (`text/css`) and woff2 (`font/woff2`) are served from the static tree.
  `docs/port-status.md` G6 (the icon-font half of the Rust strict-CSP
  divergence) is marked resolved.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.8.10 → 0.9.0 via
`scripts/bump-version.sh minor`.

## 2026-10-25 — v0.8.10: base-image bump + rust:1.99-slim re-push, warm cache now tracks the v0.8.9 lockfile

Base-bump-only release: no source change landed on `main` since `v0.8.9`
(`git log v0.8.9..HEAD` is empty), so this release simply advances the pinned
build base to the 2026-10-25 re-bake.

- **The build base advanced to the latest `vreckan-base-build` image.** The
  `Dockerfile` build-stage pin and `.base-image-digest` move together
  (ANT-277) from `sha256:089625b2…` (the 2026-10-24 re-bake) to
  `sha256:2734691e…` (the 2026-10-25 re-bake). The daily change-check fired
  on two signals: the upstream `rust:1.99-slim` digest moved
  (`sha256:aab42391…` → `sha256:1a939748…`) — a re-push of the same
  toolchain, `rustc`/`cargo` stay at 1.99.1 (verified by running both base
  images) — and vreckan's `main` moved past the commit the prior base was
  baked against (`c4b408d`, the v0.8.8 merge → `0fca397`, the v0.8.9
  merge). The warm crates.io/git dependency cache is now baked against the
  v0.8.9 `Cargo.lock`: the only dependency delta is `crypto-bigint 0.7.5 →
  0.7.6` (the RUSTSEC-2026-0387 fix that shipped in `v0.8.9`; the new
  image's cache carries `crypto-bigint-0.7.6.crate`), so local and CI builds
  resolve the current lockfile straight from the warm cache. No user- or
  operator-visible behavior change: the runtime stage, binary, and
  configuration are unchanged.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.8.9 → 0.8.10 via
`scripts/bump-version.sh patch`.

## 2026-10-24 — v0.8.9: base-image bump + crypto-bigint 0.7.5 → 0.7.6 clears RUSTSEC-2026-0387 (ANT-541, ANT-545)

Nightly base-bump cut. Since `v0.8.8`, `main` gained exactly one source
change — the `crypto-bigint` security fix (ANT-545) — which this release
ships alongside the re-baked build base.

- **The build base advanced to the latest `vreckan-base-build` image.** The
  `Dockerfile` build-stage pin and `.base-image-digest` move together (ANT-277)
  from `sha256:d8afdab8…` (the 2026-10-23 re-bake) to `sha256:089625b2…` (the
  2026-10-24 re-bake). This re-bake fired because vreckan's `main` moved past
  the commit the current base was baked against: the daily change-check saw
  the baked lockfile still tracking the v0.8.6-era commit (`13a3543`) while
  `main` was at the `v0.8.8` merge (`c4b408d`). The warm crates.io/git
  dependency cache is now baked against the current `v0.8.8` `Cargo.lock` —
  the only dependency delta is `ctutils 0.4.3 → 0.4.5` (the RUSTSEC-2026-0385
  fix that landed in `v0.8.7`), so local and CI builds resolve the current
  lockfile straight from the warm cache. The upstream `rust:1.99-slim` digest
  is unchanged (`sha256:aab42391…`) — `rustc`/`cargo` stay at 1.99 and no
  toolchain layers moved.

- **`crypto-bigint 0.7.5 → 0.7.6` clears RUSTSEC-2026-0387** (ANT-545,
  PR #92). The 2026-10-23 RustSec advisory flags constant-time timing
  variability in operations meant to be constant-time (secret-dependent
  branches in subtraction / zero counting / division / Montgomery
  multiplication, early-terminating `UintRef` equality, branching
  `BoxedMontyForm::invert`; RustCrypto/crypto-bigint#1365). The tree's 0.7.5
  instance (parented by `rsa 0.10.0-rc.19` and `crypto-primes 0.7.2`) bumps
  in place — no other dependency moves. The second flagged instance, 0.5.5,
  is pinned by `elliptic-curve 0.13.8` via `p256`/`p384 0.13` (`jsonwebtoken`
  10/11) with no 0.5.x patch out until the ecosystem ships an elliptic-curve
  0.14 line, so it rides in `deny.toml` as a documented allowance (same shape
  as the `rsa 0.9.10` / RUSTSEC-2023-0071 entry).

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.8.8 → 0.8.9 via
`scripts/bump-version.sh patch`.

## 2026-10-23 — v0.8.8: in-container audio — session-page CSP allows `data:` scripts/workers (ANT-537)

One-line: audio was dead in containers because our session-page CSP blocked
selkies' `data:`-URL AudioWorklet.

- **`SESSION_CSP` — the loose CSP applied to reverse-proxied container session
  pages — now allows `data:` in `script-src` and `worker-src`.** The selkies
  app container initializes its playback audio pipeline through an AudioWorklet
  module loaded from a `data:text/javascript` URL. The old policy
  (`script-src 'self' 'unsafe-inline' blob:` / `worker-src 'self' blob:`) was
  written for noVNC-style pages and never anticipated a `data:`-URL worklet, so
  the browser refused the module, `AudioWorklet.addModule` failed with
  `DOMException: The operation was aborted`, and the app logged
  "AudioDecoderWorker not ready". This was a gap in our CSP, not in selkies.

- **Scope: the session-page policy only.** The strict default CSP (all
  main-app pages) and the nonce-based strict CSP (collaboration room, public
  password page) are untouched — no `data:` anywhere outside the third-party
  container session pages. The session page already permits `'unsafe-inline'`
  scripts, and a `data:` script on such a page is no more powerful than inline
  code (its payload is trivially base64-encodable inline), so the marginal risk
  is ~0 while audio works again.

- **`tests/test_security.rs`**: `test_session_proxy_gets_loose_csp` now also
  asserts the proxied session page's CSP allows `data:` in both `script-src`
  and `worker-src`.

`VERSION`/`Cargo.toml`/`Cargo.lock` moved 0.8.7 → 0.8.8 via
`scripts/bump-version.sh patch`. `.base-image-digest`/`Dockerfile` are
untouched — those move with the nightly base-bump.

## 2026-10-23 — v0.8.7: base-image bump + refreshed build-base cache, no source changes since v0.8.6 (ANT-501)

Nightly base-bump cut. `main` has no commits past the `v0.8.6` tag, so this is a
build-base-only patch release: the shipped image is `v0.8.6`'s source compiled on
the refreshed build base.

- **The build base advanced to the latest `vreckan-base-build` image.** The
  `Dockerfile` build-stage pin and `.base-image-digest` move together (ANT-277)
  from `sha256:11861969…` (the 2026-10-22 re-bake) to `sha256:d8afdab8…` (the
  2026-10-23 re-bake). The re-bake fired because the upstream `rust:1.99-slim`
  digest moved (`sha256:d5928c68…` → `sha256:aab42391…`); the amd64 layers that
  base contributes — the Debian trixie layer and the Rust toolchain layer — are
  byte-identical across the two images, so `rustc`/`cargo` stay at 1.99 and the
  digest moved at the manifest-list level. What actually changed is our own
  layer set: the `-sys` build-dependency layer re-resolved against the current
  Debian trixie snapshot, and the crates.io/git dependency cache re-baked against
  `main` at `13a3543`, so the warm cache now matches this release's `Cargo.lock`.
  No operator-visible behavior changes — `git log v0.8.6..HEAD` is empty.

## 2026-10-22 — v0.8.6: base-image bump + "SealSkin Apps" default store rename + `VRECKAN_SESSION_PORT` removal (ANT-492; ANT-480)

This release completes the v0.8.6 cut: the nightly base-image update that
landed on `main` (PR #82) ships together with the Rust-side ANT-480 work —
the app-store rename and the session-port variable that was metadata-only in
the Rust port.

- **The build base advanced to the latest `vreckan-base-build` image.** The `Dockerfile` build-stage pin and `.base-image-digest` were re-pointed by the nightly scan bot (PR #82) from `sha256:adc4459d…` to `sha256:11861969…`; this release is built on the new base. The `vreckan` crate version is re-synced to 0.8.6 in `Cargo.toml`/`Cargo.lock`, which the nightly's `VERSION`-only bump had left at 0.8.5.
- **The default app store is renamed from "Vreckan Apps" to "SealSkin Apps".** A fresh install seeds the default store under the new name. On existing deployments, an idempotent startup migration runs on the first start after the upgrade: it renames the "Vreckan Apps" store row to "SealSkin Apps" (store names are unique per deployment, so that row is always the seeded default — even on hosts with a custom `VRECKAN_APP_RESOURCE_PATH`), renames the cached catalog file in place (under `/data/app_stores_cache`) so the store is served from its existing cache instead of being refetched from scratch, backfills the `source` of installed apps recorded against the store, and drops the orphaned `VRECKAN_SESSION_PORT` `app_settings` row. The migration logs once; a second start is a no-op (ANT-492).
- **`VRECKAN_SESSION_PORT` is removed.** The Rust port serves the session proxy on the same listener as the API, so the variable was metadata-only: it is no longer a defined setting (it also drops out of the admin settings response), and no `VRECKAN_*` environment variable with that name is read anymore. `VRECKAN_API_PORT` (the bind port) and the `api_port` metadata setting are unchanged (ANT-492; ANT-480).

## 2026-10-21 — F2: owner-visible participant list with per-joiner revoke and a 24-hour join-token TTL, v0.8.5 (ANT-467)

Finding F2 of the 0.8.5 review round closes the board-accepted v1 tradeoff recorded in v0.8.4 (ANT-444, F2): join grants were permanent and had no owner-side revoke path — the owner's only levers were killing or recreating the session.

- **Owners can see who is in their sessions.** Each session records every admitted visitor with how they got in; the Sessions view shows a participant-count badge on your own sessions, and the session's share dialog gains a **Participants** section: each person, how they got in (Join or a share link — with which link), when they joined, and for joiners, when their join access expires. Share-link visitors are listed by username and the link they used; they stay in while the link is active, so cutting them means revoking that link, exactly as before.
- **Per-joiner revoke.** Each joiner row in the Participants section has a two-step **Revoke** that removes that one person on their next request — the session, the other participants, and every share link are untouched, and a later Join starts them a fresh grant. This also fixes the underlying revoke path for share links: revoking a link now strips the visit grants that link minted even on plain (non-promoted) sessions, where the synthetic link id never reached the durable share store.
- **Join tokens get a 24-hour TTL.** Join access is granted for 24 hours from the last (re-)join: a joiner who keeps coming back stays in (each re-join after a lapse refreshes the window), one who is away more than a day has to Join again, and the background cleanup schedule expires lapsed grants without a restart. Grants issued before this release pick up a TTL on the joiner's next (re-)join. The window is a fixed 24 hours for 0.8.5; a per-session, owner-configurable TTL is a candidate follow-up.
- **Session lists carry the participant usernames** for the session's owner only (the admin cross-user view is unchanged), which is what the badge and the modal render from — no extra round trip.

## 2026-10-21 — v0.8.5: legacy (guacamole-lite) tier behind the session proxy + multi-user security deferrals (ANT-450; ANT-444 F1–F5; ANT-472)

The pinned pre-2.0 image (`ghcr.io/linuxserver/webtop:ubuntu-kde-2e6d5ead-ls2` — the guacamole-lite tier: `guacd` + `xrdp` + a node gclient) now works behind the session-prefix proxy: the desktop materializes for the owner and for joiners and share visitors, closing the Phase 4 QA findings F-P5 (systemic: no desktop for any role) and F-P6 (the desktop data plane was not token-gated).

- **The session proxy remaps the legacy tier's paths in both directions (F-P5).** The legacy gclient serves its page at the exact `/` route and references its assets root-absolute — the page's `/public/…` includes and the `/guaclite` tunnel dial in `rdp.js`. Behind the session-prefix proxy, the prefix-intact upstream leg 404ed on the raw image (the readiness check never passed without a one-line image shim) and the browser resolved the root-absolute references against the vreckan root (404 `text/plain`; strict MIME type checking refused to execute them as JS, so the guac client never booted and no X display ever spawned). For sessions labeled `selkies_capability: "legacy"` at launch, the proxy now strips the session prefix on the upstream leg (page at `/`, statics and the tunnel at their in-container paths) and rewrites the root-absolute `/public/…` and `/guaclite` references in the text responses it serves back to the session's proxy path. Secure-mode (selkies 2.x) sessions keep the prefix-intact, unrewritten behaviour, and the readiness gate accepts a ready legacy container (three consecutive subfolder-404 / root-200-HTML polls), so the raw pinned image needs no shim.
- **The desktop data plane is now token-gated (F-P6).** The legacy gclient itself validates no token — it renders the same encrypted RDP credentials into the page for every visitor — so before this fix the URL's `?token=`/`?access_token=` gated page access at the proxy but not the desktop. The tunnel dial now resolves through the session proxy, whose cookie/token gate applies to the page and the `/guaclite` tunnel alike. Residual hardening is tracked for the image side (the prefix-aware gclient build, F-P5 candidate (b)): per-visitor RDP credentials so a view-only share visitor cannot inject input.
- **Catalog note:** the board's open question of holding the pinned digest out of the catalog until F-P5 lands is resolved by this fix — the digest's sessions are functional again (still the labeled-legacy tier: no selkies secure mode, no WebRTC, no multi-user token table).
- **The session list no longer leaks other users' launch context (F3).** `GET /api/sessions` now nulls `launch_context` (the launched file/URL) and the custom session `name` for sessions the caller cannot join; own and joinable sessions stay fully populated (ANT-444 F3; ANT-468).
- **Joining another user's session is gated on the owner's express grant (ANT-472).** Since v0.8.4 any user with app access could self-serve Join another user's session; now a share link can name the invited user (`invited_username`), and another user's session appears on the apps page — and accepts `POST /api/sessions/{id}/join` — only when the owner has granted access: a non-revoked, non-expired named link, or a persisted participant entry (grandfathered v0.8.4 joiners stay until the owner revokes). Revoking or expiring an invited link strips the invitee's participant entry everywhere the join token lives, and the revocation response names the invitee.
- **Companion 0.8.5 changes land in the same PR:** the F1 no-store join hop (ANT-466), the F4 E2EE body gate on the join POST (ANT-469), the F4-followup E2EE body gate on session recreate + stop (ANT-471), the F5 `stop_session` map prune with generation-ordered token-table re-push (ANT-470), and the F2 participant list with per-joiner revoke and the 24-hour join-token TTL (ANT-467).

## 2026-10-21: Selkies multi-user (launch-time secure mode, first-class Join, pre-2.0 detection, multi-user docs), v0.8.4 (ANT-436; ANT-437/ANT-438/ANT-440/ANT-443/ANT-449)

v0.8.4 lands the Selkies multi-user phase (plan rev 20): plain webtop sessions enter Selkies secure mode at launch, a second user can join a running session with a single action, and the catalog knows which images support which Selkies features. The release ships with the multi-user usage docs (Phase 3, landing on release day).

- **Plain sessions enter Selkies secure mode at launch (ANT-438).** At launch, vreckan probes the container's Selkies build and promotes plain sessions to secure mode whenever the image supports it: the session's 256-bit master token gates the data plane, so two users on one session URL no longer kick each other. A Selkies 401 plain-text probe answer is treated as secure; an HTML answer stays ambiguous (`463793a3`/`558f3805`).
- **Owner WebRTC input works on the stock session URL (Phase 4 fix, ANT-449).** Selkies 2.0.0's WebRTC client sources its client token only from the page URL's `?token=` param, but the stock owner URL exposed only `?access_token=`, so on TURN-configured dual-mode images the signaling side rejected the owner's WebRTC HELLO and the owner's input was dropped. The owner session URL now carries the session's access token under both params (the same value), centralized in a single construction helper used by all owner-URL sites; no protocol, client, or image change (`32bbd297`).
- **First-class Join for a second user (ANT-440).** Running sessions get a self-serve Join action: a verified user with access to the session's app mints a 128-bit participant token (full input + file capability), recorded under the joiner's real username and idempotent across vreckan restarts and session recreates. The join-confirm modal states the observed websocket hand-off (`b6b93fdc`/`d79bb568`).
- **Owner control (MK) is never lost to a joiner or to the owner's own tabs (ANT-443).** Plain sessions re-derive control on data-plane connect/disconnect, granting it only to the most recently connected controller; a disconnected owner does not hold it and viewers never claim it, so the owner is never displaced by their own activity while a second user is joined (`699eae30`).
- **Pre-2.0 image detection + catalog capability labels (ANT-437).** The webtop catalog now carries per-image `selkiesVersion` records, and the launch-time capability probe classifies pre-2.0 images, so the catalog can label which images support multi-user secure mode (`968540ec`).
- **Multi-user docs (Phase 3).** The release is accompanied by the multi-user usage documentation (owner and second-user flows, join and kick semantics), landing on the planned release day.
- **Join-token v1 tradeoff (board-accepted; ANT-444 finding F2).** In v1, join tokens have no expiry and no owner-side revoke path: the owner's only levers are killing or recreating the session. Accepted as a design tradeoff for the first multi-user release; a participant list with per-joiner revoke and/or a join-token TTL is tracked for 0.8.5.

## 2026-10-20 — v0.8.3: trusted-peer gate on `X-Forwarded-Proto` + collab-room token hygiene, base-image bump (ANT-346; ANT-344)

A patch release cut on the nightly base bump: two HTTP-surface security fixes
landed on `main` between v0.8.2 and the tag, together with the docs/website
batch of that week. The image is the v0.8.2 source plus those changes, compiled
on the re-baked build base.

- **`X-Forwarded-Proto` is now honoured only from a configured trusted proxy (ANT-346).** Before this release any client could set (or omit) the header at will, so a client that reached the origin port directly — bypassing the proxy it was "behind" — could talk the session cookie out of its `Secure` attribute: `POST /api/auth/login` set `vreckan_auth` without `Secure` whenever the request claimed `X-Forwarded-Proto: http`. The scheme now goes through the same gate ANT-276 introduced for `X-Forwarded-For` in the login limiter: the header is accepted only when the **direct TCP peer** matches `VRECKAN_TRUSTED_PROXIES` (comma-separated IPs/CIDRs); otherwise the actual connection scheme applies, which is always `https` because the server terminates TLS itself. The gate covers the session cookie's `Secure` flag on login, logout, and self-revoke, and the OIDC `redirect_uri` derived from the request's base URL (including the admin SSO "test connection" path). Regression tests pin both directions (`test_secure_cookie_forged_xfp_from_untrusted_peer`, `test_secure_cookie_xfp_honoured_from_trusted_proxy`) plus unit coverage of the gate itself; the OIDC flow tests now issue from a configured trusted peer so their derived `http://testserver` base URL still resolves. **Operator action for TLS-terminating-proxy deployments:** set `VRECKAN_TRUSTED_PROXIES` to your proxy's address or CIDR (e.g. `10.0.0.0/8, 192.168.1.5`) if you need forwarded headers honoured — the empty default trusts no proxy, which is the safe posture for a directly-exposed origin, and it is an environment setting (no admin-UI field).
- **Collaboration-room token hygiene (ANT-344).** Three items from the same review. The room's viewer-disconnect log line printed the raw live viewer token; it now logs an 8-character prefix (char-boundary safe) — enough to tell two viewers apart in a log, too short to recover the credential. The visitor-invite redirect (the `303` whose `Location` carries a freshly minted per-visit token) now ships `Cache-Control: private, no-store`, so a caching proxy in front of the server cannot hand one visitor's token to another — the same class of fix as the v0.8.1 share-redirect hardening (ANT-322 F5). And every token comparison in the collaboration room and the session proxy moved to one shared constant-time helper (`src/tokens.rs`, length folded into the byte-walk accumulator so the runtime does not depend on where the first difference is), replacing the scattered per-module `==` compares; the same commit clears the `never_loop` clippy failure that had been breaking `cargo clippy --all-targets` on `main`.
- **Token-in-page risk documented, not silently carried.** The room page still embeds the live tokens in its page JS; the accepted-risk decision (board decision D1 under ANT-310: viewers trust the server operator, and the page ships a strict per-request nonce CSP so only operator-served same-origin scripts run) is now recorded at the code site, with the hardening path named — short-lived one-time join tokens exchanged at the websocket upgrade.
- **The build base advanced to the latest `vreckan-base-build` image.** The nightly scan detected the base-build image had moved and bumped the patch version: the `Dockerfile` build-stage pin and `.base-image-digest` move together (ANT-277 lockstep) from `sha256:472e4238…` to `sha256:adc4459d…`. The bump commit (`19fb2216`) moves only `VERSION`, `Dockerfile`, and `.base-image-digest` — not `Cargo.toml`/`Cargo.lock` — so the crate version still read 0.8.0 at this tag; it was not re-synced against `VERSION` until v0.8.5 (`4eec569e`).
- **Docs and website (ANT-335/ANT-343/ANT-344/ANT-358/ANT-359).** A page-by-page walkthrough of the web app (`/docs/app-pages`) added and wired into the site nav and footer. The docs/website freshness pass landed the missing v0.8.1 entry, refined the v0.8.0 entry's sweeper claim to the share-sweeper portion (the crypto-session sweeper is still an open gap, now stated in `port-status.md`), documented the MySQL backend in setup/README/FAQ, and refreshed the current-release references plus the missing share-session feature card. The ANT-346 setting help text gained the `X-Forwarded-Proto` wording above; `AGENTS.md` documents the daemon-side bind-mount form for build/dev containers (`/srv/paperclip/data/…` — the path the Docker daemon resolves, where the in-container `/paperclip/…` view stalled a core-build run); and the site image dropped its build-time `gpgv` apt pin ahead of the `ubuntu/nginx` re-publish that bakes the CVE-2025-68973 fix in. In the ACME suite, the `cert_status` read that could land on a pooled connection still holding a pre-write WAL snapshot now retries through a short settle, bounding that `test_cert_status_reads_from_db` flake (ANT-328).

_Entry backfilled on 2026-10-23 (ANT-512): v0.8.3 shipped without a changelog
entry — found by the weekly docs-freshness check (ANT-509). The content is
reconstructed from the shipped code at the `v0.8.3` tag and the driving issues,
not from the release notes._

## 2026-10-19 — Control-page auto-update fix + admin-UI hardening, multi-user file-permission discipline, v0.8.2 (ANT-345; ANT-347/ANT-348/ANT-349)

The "Control page stuck on Loading…" bug (ANT-347) is fixed on both ends:
the server no longer holds the auto-update job's write lock while sleeping
between ticks, and the admin UI now paints immediately, treating a slow or
failing `auto_update_status` endpoint as non-fatal. The release also ships
the multi-user file-permission discipline from finding B of the October
monthly security posture review (ANT-336) — minor severity, exploitable
only by other local users on the host, which is not the default v1
deployment.

- **The Control page no longer sticks on "Loading…" behind the auto-update check.** `GET /api/admin/apps/auto_update_status` reads the job's state under a read lock, but the background auto-update job held the matching **write** lock for the entire update interval (default one hour) while sleeping between ticks — so the endpoint stayed pending for almost the whole interval and the Control page's `Promise.all` kept the whole panel on "Loading…". The job now drops the write lock immediately after recording `last_check_at` / `next_check_at`, and the status endpoint — the only reader of that state — answers promptly again (reported in ANT-347; no API or schema change).
- **The admin UI no longer waits on the auto-update status to paint.** `renderApps()` now paints the installed-apps table and store cards from `installed`/`stores` first; `auto_update_status` is non-fatal and fills the status line and the enable/interval controls when it lands, falling back to the code defaults and an "Auto-update status unavailable" line while it is pending or on error. A late arrival never clobbers a value the admin saved in the meantime (ANT-349).
- **A "Checking for updates…" toast makes a slow status fetch visible.** While the `auto_update_status` request is merely slow (not failed), the Control page shows a sticky lower-right toast and keeps the status line neutral instead of claiming the status is unavailable; both clear the moment the response lands (ANT-349, board direction on ANT-347).
- **The live SQLite database is tightened to 0600 at boot.** When the server
  connects with a SQLite URL, the database file and its WAL/SHM sidecars are
  best-effort `chmod 0600`, so other local users can no longer read users,
  sessions, and auth tokens straight off disk. The tightening is best effort:
  a non-writable directory cannot fail boot.
- **Per-user upload directories are always tightened to 0700.** The
  per-upload directory created at upload initiation is now `chmod 0700`
  whether it was just created or already existed; previously the tightening
  only ran in the error branch, so a pre-existing directory kept its umask
  permissions and another local user could watch a user's in-flight uploads.
- **Private keys are created owner-only from the first byte.** The E2EE
  server key and the self-signed dev TLS key are now created with `O_CREAT`
  mode `0600` instead of being written at umask permissions and chmodded
  afterwards, closing a transient window another local user could race on a
  multi-user host (same fix class as ANT-274/ANT-275).

No behaviour change for single-user deployments: every tightening is
best-effort, so a read-only or non-writable directory degrades to a log line
instead of failing boot.

## 2026-10-19 — Share-session link hardening (no-store share redirect, browse-gated file root) and build-base bump, v0.8.1 (ANT-322)

A follow-up hardening release for the v0.8.0 share-session links: it fixes
the two findings (F5/F6) from the v0.8.0 review, plus the scheduled nightly
re-point of the pinned build base.

- **The share redirect is no longer cacheable.** The 303 that
  `GET /share/{id}` answers (into the app, with the fresh per-visit token in
  the target URL) now carries `Cache-Control: private, no-store`, so a
  caching proxy in front of the server cannot hand one visitor's token to
  another (F5; `src/session_shares.rs`).
- **The no-slash `api/files` root joins the browse gate.** The container
  301s the no-slash file API root to the slash-terminated listing; that
  redirect previously sat outside the session-proxy's capability check, so a
  share visitor without the file-browse cap saw the 301 instead of the
  browse 403. The root is now gated like any listing (F6;
  `src/session_proxy.rs`).
- **Regression tests** cover both fixes
  (`test_share_redirect_no_store_cache_header`,
  `test_noslash_files_root_browse_gate`).
- **Build-base bump.** The nightly re-pointed the pinned `vreckan-base-build`
  digest (rust:1.99-slim) in both `.base-image-digest` and the `Dockerfile`
  build-stage pin, per the digest convention in `docs/ops.md`.

## 2026-10-18 — Share-session links: full & view-only share links with per-link capabilities, password, expiry and revoke, plus the room-mode display fix, v0.8.0 (ANT-303; ANT-304/ANT-305/ANT-306/ANT-307)

You can now share a running app session with anyone — no Vreckan account
needed. Each running session gets a **Share** action: create a link with a
role (**Full** — they can use the app — or **View-only** — they can watch,
no input), per-capability file access (browse / download / upload), an
optional password, and an optional expiry; active links can be edited and
revoked individually, without stopping the session.

- **Managed share links.** Recipients open `/share/{id}` with no account;
  a password-protected link shows the password page first. Each visit mints
  a fresh per-visit token, so two people opening the same link hold
  different tokens, and revoking or expiring a link strips every token that
  link ever minted — one leaked link no longer lights up everything. A
  **full** link on a plain session lets the recipient drive the app; a
  **view-only** link refuses their input server-side (v1 scope: room and
  secure-mode sessions — on a plain legacy session the viewer role rides
  the `?role=` query parameter and the input refusal is enforced
  client-side, an accepted v1 limitation), and the file capability
  toggles are enforced by the session proxy on every file request
  (ANT-304/ANT-305/ANT-306).
- **Lazy secure-mode promotion.** The first share visit to a plain session
  promotes it to Selkies secure mode (a master token is minted and the
  token table is pushed into the container); on older base images without
  the token control plane the session stays legacy and the recipient's role
  rides a `?role=` query parameter instead, so share links degrade
  gracefully across app images (ANT-305).
- **Room-mode display fix.** Room invite links (participant and read-only
  viewer) now show the live display for invitees: the session proxy accepts
  the room's collaboration tokens — the controller token or a viewer's —
  over HTTP and WebSocket, instead of only the owner's session cookie.
  Stopping a room now notifies its viewers ("session ended") instead of
  leaving a dead iframe (ANT-304).
- **The expired-share sweeper is finally scheduled.** The share-cleanup job
  runs on the `share_cleanup_interval_seconds` schedule and covers both
  file shares and session shares, so expired links die on their own (the
  share-sweeper portion of port-status gap G2 — the gap itself stays open
  for the missing crypto-session sweeper; ANT-305).
- **Quick links stay documented.** The zero-setup options remain: the
  session URL itself (full access), `#shared` (server-enforced view-only),
  and `#player2`–`#player4` (gamepad co-play). The new site page
  "Sharing a session" documents all three sharing methods, the trust model
  (the server operator is in the middle of the app stream; E2EE covers the
  management API only), and the security notes (the link is a capability —
  send it over a trusted channel; the per-visit token sits in the URL only
  until the first redirect strips it) (ANT-308).
- **API docs.** `docs/api.md` and the site API reference document the new
  routes with request/response shapes: owner CRUD on
  `/api/sessions/{id}/shares` (E2EE) and the account-less
  `GET/POST /share/{id}` entry (ANT-308).
- **Website build-base bump.** The site's nginx runtime base moved from
  1.27 (alpine 3.21) to 1.30 (alpine 3.24) and is now pinned by the
  tag-level digest in `site/.base-image-digest` instead of the floating
  tag — clearing the nightly Trivy findings on the site image (ANT-315,
  following the ANT-277 digest convention).

## 2026-10-16 — Installed Apps image identity, scheduled auto-update, and the ANT-267 security batch, v0.7.0 (ANT-266/ANT-268/ANT-269/ANT-270 + ANT-273/ANT-274/ANT-275/ANT-276/ANT-277)

The Apps Control **Installed Apps** table now shows each app's real image
identity instead of "— unknown", and the update workflow (Check / Update /
scheduled auto-update) drives what the table displays. The release also
carries the ANT-267 weekly security batch.

- **The Image column stops saying "— unknown" after a server restart.** At
  startup the server inspects each installed app's local image (deduped,
  concurrently, best-effort) and caches its sha/digests, so the table shows
  the real image identity with no user action. A missing image or a down
  Docker daemon is logged and left unknown — startup never blocks, and the
  warm-up never touches pull bookkeeping (ANT-269).
- **"Check" and "Check all" now refresh the table.** The update-check
  endpoints persist the refreshed sha/digests — and the registry
  comparison — into the metadata cache the table renders from, so the
  result of a Check is visible in the list instead of vanishing (ANT-269).
- **The check toast reports the real image short id.**
  `get_local_image_info` returns the Docker-style 12-character `short_id`
  (image id minus its `sha256:` prefix); against a real daemon the Check
  toast's `current_sha` is no longer null (ANT-269).
- **Launch-time pulls are cached too.** When a launch pulls a missing image
  on demand, the pulled image's metadata is cached, so the table becomes
  accurate without admin action (ANT-269).
- **"Update available" is visible per app.** The check endpoints record
  `update_available` and `last_update_checked_at` per app; the list endpoint
  stays cache-only (both fields are `null` until a check has run — no live
  registry calls during a table render). The UI shows an amber Update button
  for apps with an available update and re-renders after a Check (ANT-270).
- **Scheduled update checks with optional auto-pull.** The previously
  unconsumed `auto_update_apps` / `auto_update_interval_seconds` settings now
  drive a background job: the first tick runs immediately at startup (local
  metadata warm-up plus a registry digest comparison), each tick re-reads
  the settings fresh (admin edits apply without a restart), the tick no-ops
  when auto-update is off, and with it on the job pulls opted-in apps that
  have an available update through the same background path as the manual
  Update button (live containers pick up the new image at their next
  start/recreate, as with a manual pull). A new read-only
  `GET /api/admin/apps/auto_update_status` exposes the job's state —
  `enabled`, `interval_seconds`, `last_check_at`, `next_check_at`,
  `updates_available`, `auto_update_apps_count` (ANT-269/ANT-270).

### Security (ANT-267 weekly batch)

- **Backups no longer carry live ACME/OIDC secrets** (major, ANT-273).
  `Db::dump` redacts the secret `app_settings` rows — the ACME account key,
  the Cloudflare token, the DNS provider config, the OIDC client secret —
  to a `<redacted>` placeholder, and `Db::restore` skips a placeholder row
  (older dumps with plaintext values still restore as-is). The ACME
  app-side cert dir now derives from the configurable data root
  (`{data_root}/acme`) instead of the hardcoded `/data/acme`, so moving
  `data_root` no longer silently loses the persisted account key and
  cert mirror on container recreation.
- **ACME key files land 0600 from the first byte** (major, ANT-274). The
  ACME account key is created owner-only (create-new with mode `0600`)
  instead of write-at-umask-then-chmod; replacing an existing file
  (e.g. one left world-readable by an older version) re-tightens it to
  `0600`, and a failed chmod is logged instead of swallowed. The on-disk
  cert/key mirror (`live/{domain}/cert.pem` + `privkey.pem`) uses the same
  owner-only writer, so the private key TLS serves no longer sits at the
  process umask.
- **The admin ACME surface stops leaking secrets** (minor, ANT-275).
  `GET /api/admin/certificates` masks `acme_dns_provider_config` (the raw
  JSON carries the provider's `api_token` / `client_secret` — blank on save
  keeps the stored value, like the Cloudflare token) and elides the
  embedded `status.key` private-key PEM for DB-sourced records. Custom CA
  directory URLs must use `https://` unless the new `acme_allow_http_ca`
  setting explicitly opts into plaintext; echoed acmex errors are capped at
  512 chars with the (possibly internal) CA URL masked; the `acmex` git
  fork is pinned to the `vreckan-2026-10-16` tag instead of a mutable branch
  rev; the self-signed dev key lands 0600 with an explicit 3-year validity
  and an expiry sidecar, so an expired dev cert is regenerated instead of
  being served forever.
- **The login limiter no longer trusts attacker-rotatable headers** (minor,
  ANT-276). The leftmost `X-Forwarded-For` entry is honoured for the
  per-IP login lockout only when the direct TCP peer matches the new
  `trusted_proxies` setting (comma-separated IPs/CIDRs; fail closed when
  empty); the lockout key lowercases the username so case variants of the
  same account share one budget; the still-on-default-password 401
  collapses to the generic "Invalid username or password." body under the
  default `warn` policy (the specific body remains only under an explicit
  `silent` opt-in); `save_sso_config` returns a generic 500 detail and logs
  the raw DB error. The clippy lint backlog across `src/` and the test
  suite is cleared in the same batch.
- **The Dockerfile build stage pins the base-build image by digest** (minor,
  ANT-277). The build stage no longer pulls a mutable
  `vreckan-base-build:latest`; the pin is recorded in
  `.base-image-digest`. The v0.7.0 image builds on the advanced base
  (digest `sha256:c4fa6f98750e…`); no operator action needed.

### Also in this release (merged to `main` since the v0.6.5 tag)

- **OIDC id-token verification adapted to jsonwebtoken v11** (ANT-246):
  the renovate bump of the `jsonwebtoken` crate lands with the signature
  changes the new major requires, so SSO sign-in is unaffected.
- Docs freshness pass: port-status test-parity notes (ANT-256) and the
  weekly doc/site corrections (ANT-242) ride the v0.7.0 image; the new
  repo `AGENTS.md` records the GitHub @-mention rule for agent comments
  (ANT-260).
- **The site image tracks `main` automatically.** A push to `main` that
  changes `site/` now dispatches the site build workflow, so the
  `vreckan-site` image in the Gitea registry stays current without a
  manual trigger (ANT-278).

## 2026-10-16 — Launch options move into the per-app Launch dialog; session name; first-sign-in password prompt, v0.6.5 (ANT-228/ANT-229/ANT-230/ANT-253)

The "Launch options" card at the top of the Apps view is gone. Picking an
app now opens a per-app **Launch dialog** that shows exactly the fields
that app type can use, and every launch option is set there, at launch
time. Verified against the T3 QA matrix (11 cases, live server).

- **One dialog for every app type.** Browser apps show a **Session name**
  field plus **URL to open** (required); file-capable apps show an
  **Open file on launch** section (upload a local file, or open a file
  already in shared storage — with an *Upload a file to shared storage*
  button when persistent storage is enabled); plain apps show the session
  name and launch options only.
- **Session name for browser sessions.** The optional session name
  appears in the session's window title and the active-sessions list, so
  a browser app pointed at different sites produces distinguishable
  sessions.
- **Moved controls.** Home directory (auto / cleanroom / a named home,
  with a *Manage* link to the home-directory list), language, GPU (shown
  only for GPU-capable apps on a server that reports GPUs), room mode,
  and Wayland (on by default) now live in the dialog instead of the
  top-bar card.
- **Presets are saved from and applied by the dialog.** *Save as preset*
  in the dialog footer saves the current options for that app; saved
  presets appear in the Pinned section of the Apps view (apply / make
  default / delete), and the dialog pre-fills from your default preset
  every time it opens (falling back to the admin's global default GPU).
- **Backend.** The applications payload now includes each app's store
  `type` (`app` / `browser` / `desktop` — data only, no client branching
  yet), and `file_path` launches honor room mode like every other launch
  path.
- **The three known issues found in the T3 QA matrix are fixed in the
  fix round and confirmed by the regression re-run** (5/5 pass, including
  both launch smoke paths — nothing previously green regressed; the
  re-run executed against this tree while it still carried the v0.6.4
  label, before the 0.6.5 bump in this entry). None of them remains a
  known issue for v0.6.5.
- **Wayland off now reaches the container env.** Unchecking Wayland in
  the Launch dialog sets `PIXELFLUX_WAYLAND=false` on the launch,
  overriding the `true` that linuxserver images bake into the image env
  (ANT-234).
- **The web client honors `must_change_password` at first sign-in.**
  While an account still carries its initial (bootstrap) password the
  server reports `must_change_password` in the login and `/me`
  responses; the client now opens the change-password dialog right after
  sign-in (form login or session restore) with a note explaining why.
  Closing it never traps the user — `VRECKAN_DEFAULT_PASSWORD_POLICY`
  still governs logins with the literal default password — and SSO
  accounts, whose password the identity provider manages, are skipped.
  This restores the long-documented "prompts for a new password at first
  sign-in" behavior, and the usage docs are aligned with the actual
  launch/Wayland/certificate behavior (ANT-253).
- **Explicit GPU selection no longer fails with a 500 in containerized
  deployments.** When the server's `/dev/dri` is bind-mounted at a
  different host path, the selected device is now remapped onto the real
  host path on the host side while the container keeps seeing the
  standard `/dev/dri` node, so an explicit pick launches cleanly
  (ANT-234).
- **Deleting your default preset resets the dialog.** After deleting your
  default preset, the next time the Launch dialog opens it restores the
  defaults: home `auto`, room off, Wayland on (language reverts to the
  browser guess, GPU to `Auto`) (ANT-235).
- **Build base advanced to digest
  sha256:1a5f145d0b719f9fefd939e6d56d46592e81a18a9d3606ba7f8ce19f7a15792d.**
  The `.base-image-digest` re-point landed on `main` after the v0.6.4 tag
  (stable `vreckan-base-build` base image, ANT-203), so the v0.6.5 image
  builds on the new base; no operator action needed (ANT-254).

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
- **`acmex` pinned to the project's `vreckan-project/acmex` fork** of the
  upstream repo (relocated out of the maintainer's personal `antitux/acmex`
  account, ANT-284)
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

**Upgrading:** pull the new image and `docker compose up -d`. The old
`VRECKAN_LE_*` env vars are **not** read by the Rust server — set the
`VRECKAN_ACME_*` names instead, and re-save any settings you had changed
from the admin Certificates page so they land under the new keys (rows
stored under the old names are not auto-migrated). The `/data/le/`
directory is no longer used; the cert mirror lives at `/data/acme/`.
(ANT-242, 2026-10-16: re-checked the current tree — the originally claimed
`VRECKAN_LE_*`→`VRECKAN_ACME_*` env fallback and `le_*`→`acme_*` key
migration are not implemented; flagged to the owner for a follow-up fix
or doc correction.)

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
