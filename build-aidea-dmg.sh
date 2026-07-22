#!/usr/bin/env bash
# =============================================================================
# build-aidea-dmg.sh — 一键打包 aidea 自包含 macOS .dmg
#
# 运行环境要求：
#   * 一台 Spotlight / mds 服务正常的健康 macOS（否则 cargo 会在依赖解析阶段
#     死锁 —— 系统 mds 损坏的典型症状：sudo mdutil -i on / 报 -400，
#     sudo launchctl load .../com.apple.metadata.mds.plist 报 I/O error）。
#   * Rust 工具链（cargo/rustc）、Node.js + npm、ollama、PostgreSQL 17 已安装。
#
# 用法：
#   bash build-aidea-dmg.sh
#
# 可选环境变量：
#   PG_BIN       PostgreSQL bin 目录（含 postgres/initdb/pg_ctl/psql）。
#                默认自动探测：先 Postgres.app，后 brew postgresql@17。
#   OLLAMA_BIN   ollama 可执行文件绝对路径（默认 command -v ollama）。
#   AIDEA_BIN    aidea release 二进制（默认 <root>/target/release/aidea）。
#   NOTARIZE=1 + APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID  开启公证（默认关闭）。
#
# 产物：gui/src-tauri/target/release/bundle/dmg/*.dmg
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

log()  { echo "[$(date +%H:%M:%S)] $*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 0) 目标三元组
# ---------------------------------------------------------------------------
TRIPLE="$(rustc -vV | sed -n 's|host: ||p')"
log "target triple: $TRIPLE"

# ---------------------------------------------------------------------------
# 1) 编译 aidea release 二进制（LTO 较慢；OOM 时自动关闭 LTO 重试）
# ---------------------------------------------------------------------------
log "==> [1/5] 构建 aidea release 二进制"
AIDEA_BIN="${AIDEA_BIN:-$ROOT/target/release/aidea}"
if cargo build --release -p aidea > /tmp/aidea_build.log 2>&1; then
  log "aidea 构建成功"
elif CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=256 \
     cargo build --release -p aidea > /tmp/aidea_build2.log 2>&1; then
  log "aidea 构建成功（已关闭 LTO 回退）"
else
  fail "aidea 构建失败，详见 /tmp/aidea_build2.log"
fi
[ -x "$AIDEA_BIN" ] || fail "aidea 二进制缺失：$AIDEA_BIN"

# ---------------------------------------------------------------------------
# 2) 拉取 sidecar 二进制（aidea / ollama / postgres / initdb / pg_ctl / psql）
# ---------------------------------------------------------------------------
log "==> [2/5] 拉取 sidecar 二进制"
if [ -z "${PG_BIN:-}" ]; then
  if [ -d /Applications/Postgres.app/Contents/Versions/17/bin ]; then
    PG_BIN=/Applications/Postgres.app/Contents/Versions/17/bin
    log "PG_BIN 自动探测 -> Postgres.app (17)"
  elif command -v brew >/dev/null 2>&1; then
    PG_BIN="$(brew --prefix postgresql@17 2>/dev/null)/bin" || true
    [ -n "$PG_BIN" ] && log "PG_BIN 自动探测 -> brew postgresql@17"
  fi
fi
[ -n "${PG_BIN:-}" ] || fail "找不到 PostgreSQL bin 目录，请设置 PG_BIN 环境变量"
[ -x "$PG_BIN/postgres" ] || fail "PG_BIN 指向的目录缺少 postgres：$PG_BIN"

PG_BIN="$PG_BIN" OLLAMA_BIN="${OLLAMA_BIN:-}" AIDEA_BIN="$AIDEA_BIN" \
  bash gui/scripts/fetch-binaries.sh

# 校验 6 个 sidecar 都存在且带正确后缀
BIN_DIR="$ROOT/gui/src-tauri/bin"
for tool in aidea ollama postgres initdb pg_ctl psql; do
  f="$BIN_DIR/$tool-$TRIPLE"
  [ -x "$f" ] || fail "sidecar 缺失或不可执行：$f"
done
log "6 个 sidecar 校验通过"

# ---------------------------------------------------------------------------
# 3) 前端依赖安装与构建（dist 已存在可跳过，但 npm install 需拉 @tauri-apps/cli）
# ---------------------------------------------------------------------------
log "==> [3/5] 安装并构建前端"
cd "$ROOT/gui"
npm install
npm run build
cd "$ROOT"

# ---------------------------------------------------------------------------
# 4) Tauri 构建 -> .app + .dmg（ad-hoc 签名；NOTARIZE=1 时开启公证）
# ---------------------------------------------------------------------------
log "==> [4/5] 构建 aidea GUI（tauri build -> .dmg）"
export TAURI_NOTARIZE="${NOTARIZE:-0}"
if [ "${NOTARIZE:-0}" = "1" ]; then
  log "公证已开启：请确保 APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID 已设置"
fi
cd "$ROOT/gui"
if [ -x ./node_modules/.bin/tauri ]; then
  ./node_modules/.bin/tauri build "$@"
elif command -v npx >/dev/null 2>&1; then
  npx tauri build "$@"
else
  cargo tauri build "$@"
fi
cd "$ROOT"

# ---------------------------------------------------------------------------
# 5) 单元测试（验证新增的 LLM 路由逻辑：ollama / openai / mock）
# ---------------------------------------------------------------------------
log "==> [5/5] 运行单元测试"
cargo test -p ide-core -p ide-probe 2>&1 | tail -25 || \
  log "（测试有失败项，请检查上方输出；打包产物不受影响）"

# ---------------------------------------------------------------------------
# 产物校验
# ---------------------------------------------------------------------------
DMG="$(ls -1 "$ROOT"/gui/src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)"
APP="$(ls -1d "$ROOT"/gui/src-tauri/target/release/bundle/macos/*.app 2>/dev/null | head -1 || true)"
if [ -n "$DMG" ]; then
  log "打包成功 ✅"
  log "  .dmg : $DMG"
  ls -lh "$DMG"
  [ -n "$APP" ] && { log "  .app : $APP"; ls -ld "$APP"; }
else
  fail "未找到 .dmg 产物，请检查 tauri build 日志"
fi
log "DONE"
