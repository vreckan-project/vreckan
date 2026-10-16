# syntax=docker/dockerfile:1
#
# Vreckan (Rust port) — multi-stage image build.
#
#   stage 1 (build):   vreckan-base-build (ANT-203) — the self-contained
#                      build/work base image (rust:1.99-slim underneath,
#                      the -sys build deps, sccache, and a warm crates.io
#                      + git dependency cache baked in) — `cargo build --release`
#   stage 2 (test):    build stage + the `tests/` tree — CI runs
#                      `cargo test --profile test-release` inside it
#                      (kept out of prod)
#   stage 3 (runtime): debian:trixie-slim — the compiled `vreckan` binary
#                      + the web client (server/static/) + the bundled
#                      template schema (server/template_schema.yml). The
#                      process runs as the non-root `vreckan` user
#                      (UID/GID 1000) with `cap_net_bind_service` file
#                      capability so it can bind the default port 443 —
#                      see the runtime stage below (ANT-81, P1-1).
#
# No Python, no pip, no site-packages, no starlette testclient patch — the
# server is one Rust binary; TLS (ring/rustls), sqlite, and the web client
# are all self-contained.
#
# Base-image pinning (carried over from the Python image, made live in
# ANT-277): the build stage below FROMs vreckan-base-build by digest instead
# of the mutable `:latest` tag, so a re-bake (daily cron, or a Cargo.lock /
# Rust-base move) cannot swap the compiler toolchain under an image build
# without a visible diff. The digest is carried in two places, kept in
# lockstep: inline in the FROM line, and in `.base-image-digest`, the
# tracked record the nightly flow reads and bumps. The nightly scan workflow
# rebuilds when the live registry digest of that base differs from the
# `vreckan.base-digest` label of the last nightly image, and its bot PR
# re-points both (the file and this FROM line) — digest bumps land as
# explicit, reviewable PRs instead of silent `latest` movement. The runtime
# base (debian:trixie-slim, the same trixie distro the build stage is built
# on, so the glibc-linked binary runs as-is) is a fixed tag, not part of the
# digest convention.

# The build base is the self-contained vreckan-base-build image (ANT-203),
# not rust:1.99-slim directly. It pre-installs the system deps the -sys
# crates need and bakes a warm crates.io + git dependency cache, so a build
# here skips both the apt install and the dependency fetch (a full
# `cargo fetch` on a fresh runner) and goes straight to compiling:
#   - system deps (linux-libc-dev, pkg-config, libssl-dev, cmake, make,
#     golang, sccache) — installed in the base; the -sys requirements are
#     documented alongside the install in the vreckan-workflow repo
#     (images/vreckan-base-build/Dockerfile)
#   - /usr/local/cargo/registry + /usr/local/cargo/git — the full
#     dependency tree per the baked Cargo.lock
#   - RUSTC_WRAPPER=sccache / SCCACHE_DIR=/sccache — the sccache env, set
#     in the base (restated below for self-documentation)
# The base's Dockerfile is maintained in vreckan-workflow (not this repo):
# images/vreckan-base-build/. Keep it in lockstep with this stage's
# requirements; its build-base-build Gitea workflow re-bakes and republishes
# the image whenever the dependency tree (Cargo.lock) or the Rust toolchain
# changes. Re-bakes move the `latest` tag, but not this stage: the FROM
# below pins the base by digest, and the nightly's base-bump PR re-points
# the pin (and `.base-image-digest`) when the tag moves.
FROM repo.vreckanproject.com/vreckan/vreckan-base-build@sha256:c4fa6f98750e7e305cf5fe1017e83036829a90ae991b39d9f218c28b443c5ea8 AS build
WORKDIR /app

# sccache: shared compile cache that persists across builds via a BuildKit
# cache mount (the `docker build` driver's BuildKit rejects `type=volume`
# mounts inside RUN; `type=cache` is supported). The CI test-run step also
# mounts SCCACHE_DIR so wrapper invocations there have a writable cache.
# On a cold rebuild (BuildKit layer cache invalidated, e.g. by a
# source-mirror switch) the dependency crates are still served from the
# sccache disk cache, so only changed crates are recompiled. Cuts cold
# builds by 30–70 %. (Both vars are also set by the base image; restated
# here so this file is self-documenting.)
ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/sccache

# Drift top-up: the base image already carries the full dependency tree per
# its baked Cargo.lock, so with an unchanged lockfile `cargo fetch` is a
# no-op and only the delta is downloaded after a lockfile change. Deliberate
# no `type=cache` mounts on /usr/local/cargo/registry|git — a named BuildKit
# cache starts empty on a fresh runner and, mounted over the baked cache,
# would hide it and force a full re-fetch. The baked layer is the warm
# cache; only genuinely new crates need network here.
COPY Cargo.toml Cargo.lock ./
RUN cargo fetch

# NB: no `type=cache` mount for /app/target — the compiled binary must live
# in the step's own snapshot so `COPY --from=build` finds it when this step
# is replayed from cache (a mount that outlives the snapshot breaks that).
COPY src ./src
RUN --mount=type=cache,id=sccache,target=/sccache \
    cargo build --release

# Test stage: the build stage plus the integration-test tree. CI builds
# `--target test` and runs `cargo test --profile test-release` in it (with
# the docker socket mounted for the daemon-dependent tests). The binary-only
# runtime stage below stays free of test code.
#
# `server/` comes along too: the tests boot the real server with its default
# settings, which resolve the bundled template schema and the web client at
# paths relative to the workdir (server/template_schema.yml,
# server/static) — same layout as the runtime stage.
#
# The test binaries are pre-compiled in this stage so the CI run step only
# executes tests. The test-release profile uses thin-LTO (vs the production
# profile's fat-LTO) and codegen-units=16 (vs 1), cutting the ~24 test-crate
# compiles roughly in half while keeping cross-crate optimisation. sccache
# further accelerates cold rebuilds by serving unchanged dependency crates
# from its persistent disk cache. The BuildKit cache replays this step on
# subsequent runs. The job count now scales with the host: bounded by the
# core count (a 16-core runner uses all of them instead of the old hard cap
# of 4) and by a 4GB-per-rustc memory budget (thin-LTO peaks well under the
# fat-LTO release profile that OOM-killed parallel rustc in runs 132/133),
# so a memory-pressed runner degrades to sequential codegen instead of OOMing.
FROM build AS test
COPY tests ./tests
COPY server ./server
RUN --mount=type=cache,id=sccache,target=/sccache \
    CORES=$(nproc); \
    JOBS=$(awk '/^MemAvailable:/{print int($2/1048576/4)}' /proc/meminfo); \
    [ "$JOBS" -lt 1 ] && JOBS=1; [ "$JOBS" -gt "$CORES" ] && JOBS=$CORES; \
    CARGO_BUILD_JOBS="$JOBS" cargo test --profile test-release --no-run

FROM debian:trixie-slim
RUN apt-get update -y && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates \
      libssl3t64 && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
# The compiled server binary.
COPY --from=build /app/target/release/vreckan /app/vreckan
# Web client + bundled template schema. Served from paths relative to the
# container working directory (server/static, server/template_schema.yml).
COPY server/ /app/server/

# The server runs as a dedicated non-root user (ANT-81, P1-1). UID/GID 1000
# matches the PUID/PGID the server uses for the app containers it spawns, so
# files it creates in the /data bind mount are owned by the same id those
# containers run as. `useradd`/`groupadd` ship with the Debian base (passwd
# is an Essential package); nologin keeps `docker exec` out by default.
RUN groupadd --gid 1000 vreckan && \
    useradd --uid 1000 --gid vreckan --home-dir /app --shell /usr/sbin/nologin vreckan && \
    # /data: the persistent data tree. docker-compose bind-mounts
    # ./vreckan-data here at runtime; the image-owned copy is what runs when
    # the image is booted without a volume (e.g. the CI smoke test).
    # /config: the non-persistent config tree — first-boot writes (the E2EE
    # server key at /config/ssl) must succeed non-root in both modes. (P2-1
    # moves the key under /data; until then /config must exist and be owned
    # by the runtime user.)
    mkdir -p /data /config && \
    chown -R vreckan:vreckan /data /config

# The server binds 0.0.0.0:$VRECKAN_API_PORT (default 443), and ports below
# 1024 are privileged by default. Whether a non-root process may bind them
# depends on the host's net.ipv4.ip_unprivileged_port_start sysctl (0 on
# most kernels/Debian, 1024 on RHEL/SUSE), so grant CAP_NET_BIND_SERVICE
# directly on the binary — the same pattern nginx's unprivileged image uses.
# That keeps the compose file free of a blanket `cap_add` (docs/ops.md §6).
RUN apt-get update -y && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      libcap2-bin && \
    setcap 'cap_net_bind_service=+ep' /app/vreckan && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Switch to the runtime user after every root-only step (P1-1): a compromised
# server process no longer runs as root in the container.
USER vreckan

EXPOSE 443

ENTRYPOINT ["/app/vreckan"]
