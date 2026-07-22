#!/usr/bin/env bash
# 构建自包含 .dmg（macOS）。
#
# 流程：拉取 sidecar 二进制 → 前端依赖安装与构建 → cargo tauri build。
# 公证：默认不公证；如需公证，设置 NOTARIZE=1 并提供 Apple 公证凭据环境变量
# （APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID），由 tauri 的 macOS 签名步骤消费。
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> 1) 拉取 sidecar 二进制"
bash scripts/fetch-binaries.sh

echo "==> 2) 安装并构建前端"
npm install
npm run build

echo "==> 3) 构建 aidea GUI（tauri build -> .dmg）"
export TAURI_NOTARIZE="${NOTARIZE:-0}"
if [ "${NOTARIZE:-0}" = "1" ]; then
  echo "公证已开启：请确保 APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID 已设置"
fi

cargo tauri build "$@"

echo "==> 完成。产物位于 src-tauri/target/release/bundle/dmg/"
