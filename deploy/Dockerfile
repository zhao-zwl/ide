# Agentic IDE Core — minimal private-deployment image (T10).
#
# Multi-stage: build the Rust workspace, then ship a slim runtime image with the
# `ide-core` (gRPC server) and `aidea` (CLI) binaries plus the migrations.

# ---- build stage ----
FROM rust:1.81-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p ide-core -p aidea

# ---- runtime stage ----
FROM debian:bookworm-slim
WORKDIR /app

# grpc_health_probe is the canonical readiness probe for the Core gRPC service.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && wget -q https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/v0.4.34/grpc_health_probe-linux-amd64 -O /usr/local/bin/grpc_health_probe \
    && chmod +x /usr/local/bin/grpc_health_probe \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ide-core /usr/local/bin/ide-core
COPY --from=builder /app/target/release/aidea /usr/local/bin/aidea
COPY migrations /app/migrations

EXPOSE 50051

# Default: start the gRPC Core server (driven by AIDEA_* env in compose).
CMD ["ide-core", "serve", "0.0.0.0:50051"]
