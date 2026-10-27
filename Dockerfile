FROM repo.vreckanproject.com/vreckan/vreckan-base-build@sha256:7b2cc27f8b86064c2e65a9435f1eb32c7aacb02f994011f7b3dbb6df1f2470ea AS build
WORKDIR /app
ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/sccache
COPY Cargo.toml Cargo.lock ./
RUN cargo fetch
COPY src ./src
RUN --mount=type=cache,id=sccache,target=/sccache \
    cargo build --release

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
