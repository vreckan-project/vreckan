# syntax=docker/dockerfile:1
#
# Vreckan (Rust port) — multi-stage image build.
#
#   stage 1 (build):   rust:1.99-slim (Debian trixie) — `cargo build --release`
#   stage 2 (test):    build stage + the `tests/` tree — CI runs
#                      `cargo test --release` inside it (kept out of prod)
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
      libssl-dev && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Warm the cargo cache from manifests alone, so later source-only changes
# don't re-fetch the whole dependency tree.
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo fetch

COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release

# Test stage: the build stage plus the integration-test tree. CI builds
# `--target test` and runs `cargo test --release` in it (with the docker
# socket mounted for the daemon-dependent tests). The binary-only runtime
# stage below stays free of test code.
FROM build AS test
COPY tests ./tests

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
