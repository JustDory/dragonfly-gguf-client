FROM rust:1 AS builder

# dragonfly-client (dfdaemon) pulls in librocksdb-sys which needs clang/libclang.
# protobuf-compiler is needed for tonic/prost codegen.
RUN apt-get update && apt-get install -y \
    clang libclang-dev protobuf-compiler \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace root before crates so Cargo can resolve the workspace graph.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./

# Copy every workspace crate (mirrors the tracker Dockerfile pattern).
COPY dragonfly-tracker/          dragonfly-tracker/
COPY dragonfly-client/           dragonfly-client/
COPY dragonfly-client-backend/   dragonfly-client-backend/
COPY dragonfly-client-config/    dragonfly-client-config/
COPY dragonfly-client-core/      dragonfly-client-core/
COPY dragonfly-client-init/      dragonfly-client-init/
COPY dragonfly-client-metric/    dragonfly-client-metric/
COPY dragonfly-client-p2p/       dragonfly-client-p2p/
COPY dragonfly-client-storage/   dragonfly-client-storage/
COPY dragonfly-client-util/      dragonfly-client-util/

# Build dfdaemon and dfget (both live in the dragonfly-client package).
RUN cargo build --release -p dragonfly-client

# ── Runtime image ────────────────────────────────────────────────────────────
# rust:1 now ships Debian Trixie (glibc 2.38, CXXABI_1.3.15). The runtime
# must match: bookworm-slim only has glibc 2.36 and trips the version errors.
# ubuntu:24.04 ships glibc 2.39 and GCC 13 (CXXABI_1.3.15) — both satisfied.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/dfdaemon /usr/local/bin/dfdaemon
COPY --from=builder /build/target/release/dfget    /usr/local/bin/dfget

# Ensure runtime directories exist.
RUN mkdir -p \
    /var/run/dragonfly \
    /var/lib/dragonfly/plugins/dfdaemon \
    /var/cache/dragonfly/dfdaemon \
    /var/log/dragonfly \
    /data/dragonfly \
    /data/gguf-seeds

# dfget env: peer-discovery tracker URL.
# NOTE: the env var is DRAGONFLY_P2P_TRACKER (not DRAGONFLY_TRACKER_URL).
ENV DRAGONFLY_P2P_TRACKER=https://tracker.dragonfly-gguf.dev

# dfdaemon env: seed manifest registry dir (read by the Iroh seed service).
ENV DRAGONFLY_GGUF_SEED_REGISTRY=/data/gguf-seeds

# Health (4003), upload gRPC (4000), proxy (4001), metrics (4002),
# storage TCP (4005), storage QUIC (4006).
EXPOSE 4000 4001 4002 4003 4005 4006

ENTRYPOINT ["dfdaemon"]
CMD ["--config", "/etc/dragonfly/dfdaemon.yaml", "--console"]
