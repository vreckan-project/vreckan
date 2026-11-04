FROM repo.vreckanproject.com/vreckan/vreckan-base-build@sha256:f33243e8e60c55866527475d090987b52e487abc316d60c41d29b831fc9b3ae0 AS build
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

FROM debian:trixie-slim
RUN apt-get update -y && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates \
      libssl3t64 && \
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
