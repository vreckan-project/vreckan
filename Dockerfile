FROM repo.vreckanproject.com/vreckan/vreckan-base-build@sha256:9d7216808bb2e7a67c34d99cdaa18cf129fb4485d2402597800389a2a061a57d AS build
WORKDIR /app
ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/sccache
COPY Cargo.toml Cargo.lock ./
COPY crates/vreckan-core/Cargo.toml crates/vreckan-core/Cargo.toml
COPY crates/vreckan-acme/Cargo.toml crates/vreckan-acme/Cargo.toml
COPY crates/vreckan-auth/Cargo.toml crates/vreckan-auth/Cargo.toml
COPY crates/vreckan-collab/Cargo.toml crates/vreckan-collab/Cargo.toml
COPY crates/vreckan-sessions/Cargo.toml crates/vreckan-sessions/Cargo.toml
COPY crates/vreckan-catalog/Cargo.toml crates/vreckan-catalog/Cargo.toml
COPY crates/vreckan-files/Cargo.toml crates/vreckan-files/Cargo.toml
COPY crates/vreckan-admin/Cargo.toml crates/vreckan-admin/Cargo.toml
COPY crates/vreckan-test-support/Cargo.toml crates/vreckan-test-support/Cargo.toml
RUN cargo fetch
COPY src ./src
COPY crates ./crates
RUN --mount=type=cache,id=sccache,target=/sccache \
    cargo build --release

FROM build AS test
# The integration suite lives in the per-crate tests/ directories (ANT-810),
# which the build stage's `COPY crates ./crates` already brought in; only the
# runtime resources under `server/` are added here.
COPY server ./server
RUN --mount=type=cache,id=sccache,target=/sccache \
    CORES=$(nproc); \
    JOBS=$(awk '/^MemAvailable:/{print int($2/1048576/4)}' /proc/meminfo); \
    [ "$JOBS" -lt 1 ] && JOBS=1; [ "$JOBS" -gt "$CORES" ] && JOBS=$CORES; \
    # --workspace: after the Candidate C split (ANT-770) the module unit tests
    # live in the member crates, and after the test-suite split (ANT-810) the
    # integration suite does too — a plain `cargo test` in a non-virtual
    # workspace only selects the root package (which has no test targets).
    CARGO_BUILD_JOBS="$JOBS" cargo test --workspace --profile test-release --no-run

# The runtime base is digest-pinned (ANT-277 convention): the bare
# trixie-slim tag sat on the 2026-10-27 manifest (apt 3.0.3) through the
# 2026-11-05 nightly, so Trivy's fixed CVE-2026-109518 (apt, HIGH) held
# the nightly push (run 727) until Debian flipped the tag; pinning the
# 2026-11-04 manifest (apt 3.0.4) makes the rebuild reproducible and
# clears the trip.
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
# zlib1g is installed explicitly (not left as a transitive dep) so the
# runtime tracks the Debian security fix for CVE-2026-76844 + CVE-2026-85091
# (zlib heap-buffer overflow, published 2026-11-07), fixed in
# 1:1.3.dfsg+really1.3.1-1+deb13u1. The digest-pinned trixie-slim base above
# predates that update, so apt resolves the patch from trixie-security at
# build time (ANT-849 unblock; base-image security track).
RUN apt-get update -y && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates \
      libssl3t64 \
      zlib1g && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=build /app/target/release/vreckan /app/vreckan
COPY server/ /app/server/

RUN groupadd --gid 1000 vreckan && \
    useradd --uid 1000 --gid vreckan --home-dir /app --shell /usr/sbin/nologin vreckan && \
    mkdir -p /data /config && \
    chown -R vreckan:vreckan /data /config && \
    apt-get update -y && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      libcap2-bin && \
    setcap 'cap_net_bind_service=+ep' /app/vreckan && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

USER vreckan

EXPOSE 443

ENTRYPOINT ["/app/vreckan"]
