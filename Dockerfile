# syntax=docker/dockerfile:1
#
# Vreckan (Rust port) — multi-stage image build.
#
#   stage 1 (build):   rust:1.99-slim (Debian trixie) — `cargo build --release`
#   stage 2 (test):    build stage + the `tests/` tree — CI runs
#                      `cargo test --profile test-release` inside it
#                      (kept out of prod)
#   stage 3 (runtime): debian:trixie-slim — the compiled `vreckan` binary
#                      + the web client (server/static/) + the bundled
#                      template schema (server/template_schema.yml).
#
# No Python, no pip, no site-packages, no starlette testclient patch — the
# server is one Rust binary; TLS (ring/rustls), sqlite, and the web client
# are all self-contained.
#
# Base-image pinning (carried over from the Python image): `.base-image-digest`
# holds the digest of the BUILD base image (rust:1.99-slim). The nightly scan
# workflow rebuilds when the live registry digest of that base differs from
# the `vreckan.base-digest` label of the last nightly image, and its bot PR
# re-points the file. The runtime base (debian:trixie-slim, the same trixie
# distro the build stage is built on, so the glibc-linked binary runs as-is)
# is a fixed tag, not part of the digest convention.

FROM rust:1.99-slim AS build
WORKDIR /app

# System deps needed by the -sys crates in the tree (gcc, libc6-dev and
# ca-certificates already ship with rust:slim):
#   linux-libc-dev   — kernel UAPI headers, required by the C code bundled
#                      with libsqlite3-sys and aws-lc-sys
#   pkg-config +
#   libssl-dev       — system OpenSSL for openssl-sys (the native-tls
#                      provider pulled in via openid/jwks -> reqwest)
RUN apt-get update -y && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      linux-libc-dev \
      pkg-config \
      libssl-dev \
      sccache && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# sccache: shared compile cache that persists across builds via a BuildKit
# cache mount (the `docker build` driver's BuildKit rejects `type=volume`
# mounts inside RUN; `type=cache` is supported). The CI test-run step also
# mounts SCCACHE_DIR so wrapper invocations there have a writable cache.
# On a cold rebuild (BuildKit layer cache invalidated, e.g. by a
# source-mirror switch) the dependency crates are still served from the
# sccache disk cache, so only changed crates are recompiled. Cuts cold
# builds by 30–70 %.
ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/sccache

# Warm the cargo cache from manifests alone, so later source-only changes
# don't re-fetch the whole dependency tree.
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo fetch

# NB: no `type=cache` mount for /app/target — the compiled binary must live
# in the step's own snapshot so `COPY --from=build` finds it when this step
# is replayed from cache (a mount that outlives the snapshot breaks that).
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=sccache,target=/sccache \
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
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=sccache,target=/sccache \
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

# Persistent data tree (database, storage, SSL keys, caches, backups).
# docker-compose bind-mounts ./vreckan-data here at runtime.
RUN mkdir -p /data

EXPOSE 443

ENTRYPOINT ["/app/vreckan"]
