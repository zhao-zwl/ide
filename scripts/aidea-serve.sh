#!/bin/sh
# aidea serve launcher for launchd.
# Waits for PostgreSQL, then starts the Core gRPC (50051) + admin HTTP (9090) server.
set -e

export AIDEA_DATABASE_URL="postgres://aidea:aidea@localhost:5432/aidea"
export AIDEA_LLM_BACKEND="local"
export AIDEA_NES_BACKEND="local"
export AIDEA_LLM_BASE_URL="http://127.0.0.1:8080/v1"
export AIDEA_LLM_MODEL="models/nes-tab/nes-tab.gguf"
export AIDEA_MODEL_NAME="models/nes-tab/nes-tab.gguf"
export AIDEA_SINGLE_TENANT="true"
export AIDEA_TENANT_ID="single"

# Diagnostics: capture panic backtraces if the server ever crashes.
export RUST_BACKTRACE=1
export RUST_LOG=info

PG_BIN=/Applications/Postgres.app/Contents/Versions/17/bin
AIDEA_BIN=/Users/zhaowenlong/IDE/ide-m1/target/debug/aidea

echo "[aidea-serve] waiting for PostgreSQL on :5432 ..."
until "$PG_BIN/pg_isready" -h localhost -p 5432 >/dev/null 2>&1; do
  sleep 1
done
echo "[aidea-serve] PostgreSQL ready. Starting aidea serve on 127.0.0.1:50051 ..."
exec "$AIDEA_BIN" serve 127.0.0.1:50051
