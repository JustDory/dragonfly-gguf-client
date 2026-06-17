FROM rust:1.88 AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY dragonfly-tracker/ dragonfly-tracker/
COPY dragonfly-client/ dragonfly-client/
COPY dragonfly-client-backend/ dragonfly-client-backend/
COPY dragonfly-client-config/ dragonfly-client-config/
COPY dragonfly-client-core/ dragonfly-client-core/
COPY dragonfly-client-init/ dragonfly-client-init/
COPY dragonfly-client-metric/ dragonfly-client-metric/
COPY dragonfly-client-p2p/ dragonfly-client-p2p/
COPY dragonfly-client-storage/ dragonfly-client-storage/
COPY dragonfly-client-util/ dragonfly-client-util/

RUN cargo build --release -p dragonfly-tracker

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/dragonfly-tracker /usr/local/bin/dragonfly-tracker
EXPOSE 8080
ENTRYPOINT ["dragonfly-tracker"]
CMD ["--bind", "0.0.0.0:8080"]
