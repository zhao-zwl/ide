#!/usr/bin/env bash
# =============================================================================
# package-dmg.sh — 在本机 macOS 上把 CI 交叉编译产物封装成 aidea.dmg
#
# 【为什么需要这个脚本】
#   本机 macOS 的 Spotlight 元数据服务（mds）已损坏，任何形式的 `cargo build`
#   （含 `--offline`）都会在依赖解析阶段死锁，因此无法在本机编译。
#   而 `hdiutil` / `codesign` / `ditto` / `sips` **不依赖 mds**，可以正常工作。
#   所以：编译在 GitHub Actions 的 Linux runner 上用 osxcross 交叉完成，
#         封包（.app 组装 + ad-hoc 签名 + dmg 制作）在本机由本脚本完成。
#
# 【用法】
#   1) 打开 GitHub 仓库 → Actions → "Build aidea macOS bundle (cross-compile)"
#      → Run workflow（或等 push 到 main 自动触发）。
#   2) 运行结束后在该次运行的 Artifacts 区下载 `aidea-bundle`（zip）。
#   3) 解压得到 `aidea-bundle.tar.gz`，与本脚本放在同一目录，然后执行：
#
#         bash package-dmg.sh aidea-bundle.tar.gz
#
#      （不带参数时会自动在当前目录寻找 aidea-bundle.tar.gz）
#   4) 完成后当前目录得到 `aidea.dmg`。
#
# 【可选环境变量】
#   OUT_DIR=<dir>            输出目录（默认当前目录）
#   OUT_NAME=<name>          输出文件名（默认 aidea.dmg）
#   DRY_RUN=1                只解压 + 组装 .app 目录结构并打印，不签名/不制作 dmg。
#                            可在非 macOS 上运行，用于校验目录拼装逻辑。
#   KEEP_WORK=1              保留临时工作区（打印路径），便于排查。
#   MIRROR_MACOS_BIN=0       不在 Contents/MacOS/ 下创建 sidecar 兼容软链（默认创建）。
#   SKIP_SIGN=1              跳过 ad-hoc 签名（不推荐，仅排障用）。
#
# 【本脚本绝不调用】cargo / rustc / tauri / npm / mdfind / mdutil —— 不会触发 mds。
# =============================================================================
set -euo pipefail

# ---------------------------------------------------------------------------
# 常量：必须与 gui/src-tauri/tauri.conf.json 及 Rust 侧路径解析保持一致
# ---------------------------------------------------------------------------
readonly APP_NAME="aidea"
readonly BUNDLE_ID="com.aidea.gui"
readonly APP_VERSION="0.1.0"
readonly MIN_OS="10.15"
readonly TRIPLE="x86_64-apple-darwin"
readonly MAIN_BIN="aidea-gui"

# 6 个 sidecar：Rust 侧 `bootstrap::sidecar::bin_path()` 解析为
# `resource_dir()/bin/<name>`，即 aidea.app/Contents/Resources/bin/<name>（无 triple 后缀）。
SIDECARS=(aidea ollama postgres initdb pg_ctl psql)
# 缺失即无法启动的关键 sidecar（其余缺失只告警、降级运行）
CRITICAL_SIDECARS=(aidea)

DRY_RUN="${DRY_RUN:-0}"
KEEP_WORK="${KEEP_WORK:-0}"
SKIP_SIGN="${SKIP_SIGN:-0}"
MIRROR_MACOS_BIN="${MIRROR_MACOS_BIN:-1}"
OUT_DIR="${OUT_DIR:-$PWD}"
OUT_NAME="${OUT_NAME:-${APP_NAME}.dmg}"

# ---------------------------------------------------------------------------
# 日志工具
# ---------------------------------------------------------------------------
log()  { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
warn() { printf '[%s] WARN: %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# 跨平台复制：优先 ditto（保留 macOS 元数据/符号链接），回退 cp -R。
copy_tree() {
  local src="$1" dst="$2"
  [ -e "$src" ] || return 1
  mkdir -p "$(dirname "$dst")"
  if command -v ditto >/dev/null 2>&1; then
    ditto "$src" "$dst"
  else
    rm -rf "$dst"
    cp -R "$src" "$dst"
  fi
}

copy_file() {
  local src="$1" dst="$2"
  [ -f "$src" ] || return 1
  mkdir -p "$(dirname "$dst")"
  cp -f "$src" "$dst"
}

# ---------------------------------------------------------------------------
# 0) 环境检查
# ---------------------------------------------------------------------------
IS_MACOS=0
[ "$(uname -s)" = "Darwin" ] && IS_MACOS=1

if [ "$IS_MACOS" -ne 1 ] && [ "$DRY_RUN" != "1" ]; then
  fail "本脚本需在 macOS 上运行（当前: $(uname -s)）。若只想校验目录拼装逻辑，请用 DRY_RUN=1 运行。"
fi

if [ "$DRY_RUN" != "1" ]; then
  command -v hdiutil  >/dev/null 2>&1 || fail "缺少 hdiutil（macOS 自带），无法制作 dmg。"
  if [ "$SKIP_SIGN" != "1" ]; then
    command -v codesign >/dev/null 2>&1 || fail "缺少 codesign（随 Xcode Command Line Tools 提供）。请先执行：xcode-select --install"
  fi
fi

# ---------------------------------------------------------------------------
# 1) 定位 bundle tar.gz
# ---------------------------------------------------------------------------
TARBALL="${1:-}"
if [ -z "$TARBALL" ]; then
  for cand in \
      "$PWD/aidea-bundle.tar.gz" \
      "$PWD/aidea-bundle/aidea-bundle.tar.gz" \
      "$HOME/Downloads/aidea-bundle.tar.gz" \
      "$HOME/Downloads/aidea-bundle/aidea-bundle.tar.gz"; do
    if [ -f "$cand" ]; then TARBALL="$cand"; break; fi
  done
fi
[ -n "$TARBALL" ] || fail "未找到 aidea-bundle.tar.gz。用法：bash package-dmg.sh <path/to/aidea-bundle.tar.gz>"
[ -f "$TARBALL" ] || fail "文件不存在：$TARBALL"
TARBALL="$(cd "$(dirname "$TARBALL")" && pwd)/$(basename "$TARBALL")"
log "使用 bundle：$TARBALL ($(du -h "$TARBALL" 2>/dev/null | cut -f1))"

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

# ---------------------------------------------------------------------------
# 2) 解压到临时工作区
# ---------------------------------------------------------------------------
WORK="$(mktemp -d "${TMPDIR:-/tmp}/aidea-dmg.XXXXXX")"
cleanup() {
  if [ "$KEEP_WORK" = "1" ]; then
    log "保留工作区：$WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

log "==> [1/6] 解压 bundle"
tar -xzf "$TARBALL" -C "$WORK" || fail "解压失败：$TARBALL"

# bundle 内允许有一层顶层目录；定位含 payload/ 的那一层
PAYLOAD=""
while IFS= read -r d; do PAYLOAD="$d"; break; done < <(find "$WORK" -maxdepth 3 -type d -name payload 2>/dev/null | sort)
[ -n "$PAYLOAD" ] || fail "bundle 结构异常：未找到 payload/ 目录。请确认下载的是 CI 产出的 aidea-bundle.tar.gz。"
BUNDLE_ROOT="$(dirname "$PAYLOAD")"
log "payload 目录：$PAYLOAD"
if [ -f "$BUNDLE_ROOT/MANIFEST.txt" ]; then
  sed 's/^/    | /' "$BUNDLE_ROOT/MANIFEST.txt"
fi

# ---------------------------------------------------------------------------
# 3) 组装 .app bundle
#
#    最终结构（与 Rust 侧 bootstrap::sidecar 的路径解析严格一致）：
#      aidea.app/Contents/
#        Info.plist
#        MacOS/aidea-gui                 主二进制（CFBundleExecutable）
#        MacOS/<sidecar>                 -> ../Resources/bin/<sidecar> 兼容软链（可关）
#        Resources/bin/<sidecar>         6 个 sidecar（去掉 triple 后缀）
#        Resources/lib/                  postgres 依赖 dylib + ollama runner
#                                        （postgres/ollama 均按 <exe>/../lib 查找）
#        Resources/share/                postgres initdb 模板等（<exe>/../share）
#        Resources/resources/migrations  SQL 迁移（resource_path "resources/migrations"）
#        Resources/resources/models/...  Modelfile + 可选 qwen2.5-0.5b.gguf
#        Resources/dist/                 前端静态文件（Tauri v2 已内嵌进二进制，此处为留档）
#        Resources/icons/icon.png, Resources/icon.icns
# ---------------------------------------------------------------------------
log "==> [2/6] 组装 ${APP_NAME}.app"
STAGE="$WORK/stage"
APP_DIR="$STAGE/${APP_NAME}.app"
CONTENTS="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS/MacOS"
RES_DIR="$CONTENTS/Resources"
rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RES_DIR/bin"

# --- 主二进制 ---------------------------------------------------------------
MAIN_SRC=""
for cand in "$PAYLOAD/macos/$MAIN_BIN" "$PAYLOAD/macos/${MAIN_BIN}-${TRIPLE}" "$PAYLOAD/bin/${MAIN_BIN}-${TRIPLE}"; do
  if [ -f "$cand" ]; then MAIN_SRC="$cand"; break; fi
done
[ -n "$MAIN_SRC" ] || fail "bundle 缺少主二进制 ${MAIN_BIN}（期望在 payload/macos/${MAIN_BIN}）。CI 的交叉编译很可能失败了，请检查 Actions 日志。"
copy_file "$MAIN_SRC" "$MACOS_DIR/$MAIN_BIN"
chmod +x "$MACOS_DIR/$MAIN_BIN"
log "主二进制 -> Contents/MacOS/$MAIN_BIN"

# --- 6 个 sidecar（去掉 triple 后缀）----------------------------------------
MISSING_SIDECARS=""
for name in "${SIDECARS[@]}"; do
  src=""
  for cand in "$PAYLOAD/bin/${name}-${TRIPLE}" "$PAYLOAD/bin/${name}"; do
    if [ -f "$cand" ]; then src="$cand"; break; fi
  done
  if [ -n "$src" ]; then
    copy_file "$src" "$RES_DIR/bin/$name"
    chmod +x "$RES_DIR/bin/$name"
    log "sidecar -> Contents/Resources/bin/$name"
  else
    MISSING_SIDECARS="$MISSING_SIDECARS $name"
    warn "sidecar 缺失：${name}（bundle 中无 bin/${name}-${TRIPLE}）"
  fi
done

for name in "${CRITICAL_SIDECARS[@]}"; do
  case " $MISSING_SIDECARS " in
    *" $name "*) fail "关键 sidecar 缺失：$name —— 无法产出可用的 .app。请检查 CI 交叉编译日志。" ;;
  esac
done
if [ -n "$MISSING_SIDECARS" ]; then
  warn "以下 sidecar 缺失，对应功能将降级：$MISSING_SIDECARS"
fi

# --- postgres / ollama 的运行时依赖（lib / share）----------------------------
# postgres、psql、ollama 都用 @loader_path/.. 或 <exe>/.. 相对定位依赖，
# 因此 bin 与 lib/share 必须是同级兄弟目录（都在 Contents/Resources 下）。
if [ -d "$PAYLOAD/lib" ]; then
  copy_tree "$PAYLOAD/lib" "$RES_DIR/lib"
  log "运行时库 -> Contents/Resources/lib ($(find "$RES_DIR/lib" -type f 2>/dev/null | wc -l | tr -d ' ') 个文件)"
else
  warn "bundle 无 lib/ —— postgres/ollama 可能因缺少 dylib 无法启动"
fi
if [ -d "$PAYLOAD/share" ]; then
  copy_tree "$PAYLOAD/share" "$RES_DIR/share"
  log "共享数据 -> Contents/Resources/share"
else
  warn "bundle 无 share/ —— initdb 可能因缺少模板无法初始化数据库"
fi

# --- Tauri resources（migrations / models）----------------------------------
if [ -d "$PAYLOAD/resources" ]; then
  copy_tree "$PAYLOAD/resources" "$RES_DIR/resources"
  log "Tauri 资源 -> Contents/Resources/resources"
  if [ -f "$RES_DIR/resources/models/nes-tab/qwen2.5-0.5b.gguf" ]; then
    log "  端模型权重已内置（离线可用）"
  else
    warn "  未内置 qwen2.5-0.5b.gguf —— 首次启动需联网让 ollama 拉取基础权重"
  fi
else
  warn "bundle 无 resources/ —— 数据库迁移与端模型 Modelfile 将缺失"
fi

# --- 前端 dist（Tauri v2 在编译期已内嵌，这里仅留档/便于排查）----------------
if [ -d "$PAYLOAD/dist" ]; then
  copy_tree "$PAYLOAD/dist" "$RES_DIR/dist"
  log "前端 dist -> Contents/Resources/dist"
fi

# --- 图标 -------------------------------------------------------------------
if [ -d "$PAYLOAD/icons" ]; then
  copy_tree "$PAYLOAD/icons" "$RES_DIR/icons"
fi

ICNS_OK=0
ICON_PNG="$RES_DIR/icons/icon.png"
if [ "$DRY_RUN" != "1" ] && [ -f "$ICON_PNG" ] \
   && command -v sips >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
  ICONSET="$WORK/${APP_NAME}.iconset"
  mkdir -p "$ICONSET"
  gen_ok=1
  for spec in "16:icon_16x16.png" "32:icon_16x16@2x.png" "32:icon_32x32.png" \
              "64:icon_32x32@2x.png" "128:icon_128x128.png" "256:icon_128x128@2x.png" \
              "256:icon_256x256.png" "512:icon_256x256@2x.png" "512:icon_512x512.png" \
              "1024:icon_512x512@2x.png"; do
    px="${spec%%:*}"; out="${spec#*:}"
    sips -s format png -z "$px" "$px" "$ICON_PNG" --out "$ICONSET/$out" >/dev/null 2>&1 || gen_ok=0
  done
  if [ "$gen_ok" = "1" ] && iconutil -c icns "$ICONSET" -o "$RES_DIR/icon.icns" >/dev/null 2>&1; then
    ICNS_OK=1
    log "图标 -> Contents/Resources/icon.icns"
  else
    warn "icon.icns 生成失败（源 png 可能过小），将使用系统默认图标"
  fi
fi

# --- Info.plist -------------------------------------------------------------
write_info_plist() {
  local dst="$1" icon_key=""
  if [ "$ICNS_OK" = "1" ]; then
    icon_key="	<key>CFBundleIconFile</key>
	<string>icon.icns</string>
"
  fi
  cat > "$dst" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>${APP_NAME}</string>
	<key>CFBundleDisplayName</key>
	<string>${APP_NAME}</string>
	<key>CFBundleIdentifier</key>
	<string>${BUNDLE_ID}</string>
	<key>CFBundleExecutable</key>
	<string>${MAIN_BIN}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleVersion</key>
	<string>${APP_VERSION}</string>
	<key>CFBundleShortVersionString</key>
	<string>${APP_VERSION}</string>
	<key>CFBundleSupportedPlatforms</key>
	<array>
		<string>MacOSX</string>
	</array>
${icon_key}	<key>LSMinimumSystemVersion</key>
	<string>${MIN_OS}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSPrincipalClass</key>
	<string>NSApplication</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
	<key>NSAppTransportSecurity</key>
	<dict>
		<key>NSAllowsLocalNetworking</key>
		<true/>
	</dict>
</dict>
</plist>
PLIST
}

# 优先使用 CI 随包发出的模板；缺失时用上面的函数生成等价内容。
if [ -f "$BUNDLE_ROOT/Info.plist" ]; then
  copy_file "$BUNDLE_ROOT/Info.plist" "$CONTENTS/Info.plist"
  log "Info.plist 取自 bundle 模板（CFBundleExecutable=${MAIN_BIN}, id=${BUNDLE_ID}）"
else
  write_info_plist "$CONTENTS/Info.plist"
  log "Info.plist 已生成（CFBundleExecutable=${MAIN_BIN}, id=${BUNDLE_ID}）"
fi

# --- Contents/MacOS 下的 sidecar 兼容软链 ------------------------------------
# Rust 侧读的是 Resources/bin/，但 Tauri 官方 bundler 的约定是 MacOS/。
# 建立相对软链两边都能命中，且不占额外磁盘空间。
create_macos_mirror() {
  for name in "${SIDECARS[@]}"; do
    [ -f "$RES_DIR/bin/$name" ] || continue
    ln -sfn "../Resources/bin/$name" "$MACOS_DIR/$name"
  done
}
remove_macos_mirror() {
  for name in "${SIDECARS[@]}"; do
    if [ -L "$MACOS_DIR/$name" ]; then rm -f "$MACOS_DIR/$name"; fi
  done
  return 0
}
if [ "$MIRROR_MACOS_BIN" = "1" ]; then
  create_macos_mirror
  log "已在 Contents/MacOS/ 建立 sidecar 兼容软链"
fi

# ---------------------------------------------------------------------------
# 4) 权限、隔离属性
# ---------------------------------------------------------------------------
log "==> [3/6] 修正可执行权限"
chmod +x "$MACOS_DIR/$MAIN_BIN"
if [ -d "$RES_DIR/bin" ]; then
  find "$RES_DIR/bin" -type f -exec chmod +x {} + 2>/dev/null || true
fi
if [ -d "$RES_DIR/lib" ]; then
  find "$RES_DIR/lib" -type f -name '*.dylib' -exec chmod +x {} + 2>/dev/null || true
  # ollama 的 runner 可执行文件也在 lib/ollama 下
  find "$RES_DIR/lib" -type f -name 'ollama*' -exec chmod +x {} + 2>/dev/null || true
fi
if [ "$IS_MACOS" = "1" ]; then
  xattr -cr "$APP_DIR" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# DRY_RUN：只打印结构，验证拼装逻辑
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" = "1" ]; then
  log "==> [DRY_RUN] 目录结构如下（不签名、不制作 dmg）"
  if command -v find >/dev/null 2>&1; then
    (cd "$STAGE" && find . -maxdepth 5 | sort | sed 's/^/    /')
  fi
  log "DRY_RUN 完成。去掉 DRY_RUN=1 并在 macOS 上重跑即可生成 dmg。"
  exit 0
fi

# ---------------------------------------------------------------------------
# 5) ad-hoc 签名（tauri.conf.json 的 signingIdentity 为 null，无开发者证书）
#    顺序必须由内向外：先签嵌套的可执行/动态库，最后签 .app 本体。
# ---------------------------------------------------------------------------
if [ "$SKIP_SIGN" = "1" ]; then
  warn "已跳过签名（SKIP_SIGN=1）。未签名的 .app 在较新 macOS 上可能无法启动。"
else
  log "==> [4/6] ad-hoc 代码签名"
  sign_one() {
    codesign --force --sign - --timestamp=none "$1" >/dev/null 2>&1 \
      || warn "签名失败（已跳过）：${1#$APP_DIR/}"
  }
  # 内层：sidecar 与动态库
  if [ -d "$RES_DIR/bin" ]; then
    while IFS= read -r f; do sign_one "$f"; done < <(find "$RES_DIR/bin" -type f)
  fi
  if [ -d "$RES_DIR/lib" ]; then
    while IFS= read -r f; do sign_one "$f"; done \
      < <(find "$RES_DIR/lib" -type f \( -name '*.dylib' -o -name '*.so' -o -perm -u+x \))
  fi
  # 外层：.app 本体
  if codesign --force --deep --sign - "$APP_DIR" >/dev/null 2>&1; then
    log "ad-hoc 签名完成"
  elif [ "$MIRROR_MACOS_BIN" = "1" ] && remove_macos_mirror \
       && codesign --force --deep --sign - "$APP_DIR" >/dev/null 2>&1; then
    warn "含 MacOS/ 软链时签名失败，已移除软链后重签成功（Rust 侧读 Resources/bin/，功能不受影响）"
  else
    fail "codesign 失败。请确认已安装 Xcode Command Line Tools（xcode-select --install）；
      也可先用 SKIP_SIGN=1 生成未签名 dmg 排查：SKIP_SIGN=1 bash package-dmg.sh \"$TARBALL\""
  fi
  codesign --verify --deep --strict "$APP_DIR" >/dev/null 2>&1 \
    && log "签名校验通过" \
    || warn "codesign --verify 未通过（ad-hoc 签名下常见，通常不影响本机运行）"
fi

# ---------------------------------------------------------------------------
# 6) hdiutil 制作 dmg（带 /Applications 拖拽软链）
# ---------------------------------------------------------------------------
log "==> [5/6] hdiutil 制作 dmg"
DMG_ROOT="$WORK/dmgroot"
rm -rf "$DMG_ROOT"
mkdir -p "$DMG_ROOT"
copy_tree "$APP_DIR" "$DMG_ROOT/${APP_NAME}.app" || fail "复制 .app 到 dmg 暂存目录失败"
ln -sfn /Applications "$DMG_ROOT/Applications"

OUT_DMG="$OUT_DIR/$OUT_NAME"
rm -f "$OUT_DMG"
if ! hdiutil create \
      -volname "$APP_NAME" \
      -srcfolder "$DMG_ROOT" \
      -fs HFS+ \
      -format UDZO \
      -ov "$OUT_DMG" >/dev/null; then
  warn "UDZO 压缩失败，回退为未压缩 UDRO 重试"
  hdiutil create -volname "$APP_NAME" -srcfolder "$DMG_ROOT" -fs HFS+ -format UDRO -ov "$OUT_DMG" >/dev/null \
    || fail "hdiutil 制作 dmg 失败"
fi
[ -f "$OUT_DMG" ] || fail "未生成 dmg：$OUT_DMG"

# ---------------------------------------------------------------------------
# 7) 收尾输出
# ---------------------------------------------------------------------------
log "==> [6/6] 完成"
echo
echo "  ✅ dmg 已生成：$OUT_DMG"
echo "     大小：$(du -h "$OUT_DMG" 2>/dev/null | cut -f1)"
echo
echo "  安装 / 首次打开："
echo "    1) 双击 ${OUT_NAME}，把 ${APP_NAME}.app 拖到 Applications。"
echo "    2) 由于是 ad-hoc 签名（无 Apple 开发者证书），首次打开若提示"
echo "       「无法打开，因为无法验证开发者」，任选一种方式放行："
echo "         · 在 Finder 里右键点击 ${APP_NAME}.app → 打开 → 再次点「打开」"
echo "         · 或执行： xattr -dr com.apple.quarantine /Applications/${APP_NAME}.app"
echo "    3) 系统设置 → 隐私与安全性 → 底部「仍要打开」也可放行。"
echo
