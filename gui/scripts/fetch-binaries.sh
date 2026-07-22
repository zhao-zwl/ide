#!/usr/bin/env bash
# 拉取/拷贝自包含打包所需的 sidecar 二进制到 gui/src-tauri/bin/。
#
# Tauri `externalBin` 期望文件名形如 `bin/<name>-<target-triple>`（例如
# `bin/aidea-x86_64-apple-darwin`）。本脚本从各来源拷贝并重命名。
#
# 依赖（在打包机上需已就绪）：
#   * aidea        —— `cargo build --release` 产物（crates/cli 的 `aidea` 二进制）
#   * ollama       —— 本地 ollama 可执行文件
#   * postgres 等  —— 来自 PostgreSQL 安装（如 `brew --prefix postgresql@17` 的 bin）
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
BIN_DIR="$ROOT/src-tauri/bin"
mkdir -p "$BIN_DIR"

# 目标三元组（macOS 通用：按本机架构选择；如需 universal 请另行 lipo 合并）。
TRIPLE="$(rustc -vV | sed -n 's|host: ||p')"
echo "target triple: $TRIPLE"

# --- aidea（本地构建）------------------------------------------------------
AIDEA_BIN="${AIDEA_BIN:-$ROOT/../target/release/aidea}"
if [ -x "$AIDEA_BIN" ]; then
  cp "$AIDEA_BIN" "$BIN_DIR/aidea-$TRIPLE"
  echo "copied aidea -> bin/aidea-$TRIPLE"
else
  echo "WARN: aidea 二进制缺失（$AIDEA_BIN），请先 cargo build --release" >&2
fi

# --- ollama（系统 PATH 或指定 OLLAMA_BIN）----------------------------------
OLLAMA_BIN="${OLLAMA_BIN:-$(command -v ollama || true)}"
if [ -n "$OLLAMA_BIN" ]; then
  cp "$OLLAMA_BIN" "$BIN_DIR/ollama-$TRIPLE"
  echo "copied ollama -> bin/ollama-$TRIPLE"
else
  echo "WARN: ollama 未找到（OLLAMA_BIN），chat 离线将不可用" >&2
fi

# --- PostgreSQL 工具链（initdb / postgres / pg_ctl / psql）-----------------
# PG_BIN 指向 PostgreSQL 的 bin 目录（例如 $(brew --prefix postgresql@17)/bin）。
PG_BIN="${PG_BIN:-}"
if [ -z "$PG_BIN" ]; then
  PG_BIN="$(brew --prefix postgresql@17 2>/dev/null)/bin" || true
fi
for tool in postgres initdb pg_ctl psql; do
  src="$PG_BIN/$tool"
  if [ -x "$src" ]; then
    cp "$src" "$BIN_DIR/$tool-$TRIPLE"
    echo "copied $tool -> bin/$tool-$TRIPLE"
  else
    echo "WARN: $tool 未找到（$src）" >&2
  fi
done

echo "done. bin 目录内容："
ls -la "$BIN_DIR"
