#!/usr/bin/env bash
# =============================================================================
# verify-payload.sh — 三大下载（llama-server / PostgreSQL / 端模型权重）的产物分项断言
#
# 【为什么需要它】
#   run #16 里 step 19 llama-server 4s、step 20 PostgreSQL 4s、step 21 模型权重 11s，
#   三步全 success，最终 artifact 586.22 MiB。但这三步的失败路径**全部是
#   `::warning::` + `exit 0`（见 build-dmg.yml）——也就是说：
#     · llama-server 下载失败   → warning，步骤仍 success，包里没有 llama-server
#     · Postgres 下载失败 → warning，步骤仍 success，包里没有数据库
#     · 权重下载失败      → warning，步骤仍 success，包里没有模型
#   586 MiB 这个总量**无法归因到具体哪一项**，所以「三步全绿」并不能证明
#   三样东西都在包里。本脚本把「隐式的 warning」变成「显式的分项断言」。
#
# 【断言策略】
#   llama-server / PostgreSQL  → 缺失即**硬失败**（后端栈必需，缺了 App 起不来）
#   模型权重             → 维持 best-effort，但**必须在注解里区分 BAKED / SKIPPED**，
#                          绝不静默 success；若文件存在却损坏（魔数错/截断）
#                          则硬失败 —— 发一个坏模型比不发更糟。
#
# 【体积下限的取法】
#   全部取「远低于真实体积」的保守值，目的只是挡住空文件 / HTML 错误页 /
#   截断 / 占位脚本，因此不存在误杀正常产物的风险。上游实测参考值：
#     llama-server (llama.cpp bXXXX, x86_64)   数十 MiB（单二进制，无额外 runner）
#     Postgres-2.9.5-17.dmg 114.0 MiB
#     qwen2.5-0.5b q4_k_m   约 400 MiB
#
# 用法:
#   verify-payload.sh <bundle_dir> <model_gguf_path> [facts_dir]
#   verify-payload.sh --self-test
#
#   facts_dir（默认 .ci-facts）：预留的下载步骤事实文件目录。llama.cpp 为单二进制，
#   无 runner 三态，本脚本不依赖它，仅保留参数以便将来需要时扩展。
# =============================================================================

set -uo pipefail   # 不开 -e：由脚本自己收敛退出码

: "${TARGET_TRIPLE:=x86_64-apple-darwin}"

# ---- 体积下限（字节）。可用环境变量覆盖，便于将来调参而不改脚本。----------
: "${MIN_LLAMACPP_BYTES:=8388608}"      #  8 MiB —— llama-server 主二进制 + 同目录 *.dylib 合计（SHARED 构建下主二进制仅 ~3MB，体积在 dylib 中；静态构建则单二进制即数十 MiB）。保守下限仅挡空文件/HTML/截断。
: "${MIN_PGTOOL_BYTES:=32768}"        # 32 KiB —— 单个 pg 可执行文件
: "${MIN_PGSHARE_BYTES:=1048576}"     #  1 MiB —— initdb 模板/时区数据
: "${MIN_PGLIB_BYTES:=1048576}"       #  1 MiB —— libpq 等 dylib
: "${MIN_MODEL_BYTES:=67108864}"      # 64 MiB —— 端模型权重

# -----------------------------------------------------------------------------
# 工具函数（POSIX 友好：du -sk 在 Linux/macOS 上行为一致，便于本地自测）
# -----------------------------------------------------------------------------
file_size() { [ -f "$1" ] || { echo 0; return; }; wc -c < "$1" 2>/dev/null | tr -d ' []' ; }
dir_bytes() { [ -d "$1" ] || { echo 0; return; }; du -sk "$1" 2>/dev/null | awk '{print $1*1024; exit}'; }
human()     { awk -v b="${1:-0}" 'BEGIN{ if (b >= 1048576) printf "%.1fMB", b/1048576; else printf "%.0fKB", b/1024 }'; }

# -----------------------------------------------------------------------------
main() {
  local BUNDLE="${1:-.bundle}"
  local MODEL="${2:-}"
  local FACTS="${3:-.ci-facts}"
  local n_fail=0

  echo "::group::payload inventory (${BUNDLE})"
  ls -lhR "$BUNDLE" 2>/dev/null | head -80 || true
  echo "::endgroup::"

  # =========================================================================
  # C2-a  llama-server —— 缺失硬失败（决策 B：替代 ollama 的本地推理后端）
  # =========================================================================
  local llamacpp_bin llamacpp_sz llamacpp_rep llamacpp_total llamacpp_libs=0 d
  llamacpp_bin="${BUNDLE}/bin/llama-server-${TARGET_TRIPLE}"
  llamacpp_sz="$(file_size "$llamacpp_bin")"

  echo "--- llama-server ---"
  ls -lh "$llamacpp_bin" 2>/dev/null || echo "  (缺失: $llamacpp_bin)"

  # SHARED 构建（T14/T16）下主二进制仅 ~3MB，体积分散到同目录 libllama*.dylib /
  # libcommon*.dylib。故以「主二进制 + 同目录全部 *.dylib」合计体积做断言：
  # 既保留对空文件 / HTML 错误页 / 截断的防护（合法 SHARED 构建必带 dylib，合计远超下限），
  # 又不会把合法的共享链接二进制误杀。静态构建无 dylib，合计即主二进制本身（数十 MiB），同样通过。
  llamacpp_total="$llamacpp_sz"
  for d in "$BUNDLE/bin"/*.dylib; do
    [ -f "$d" ] || continue
    llamacpp_libs=$((llamacpp_libs + $(file_size "$d")))
    llamacpp_total=$((llamacpp_total + $(file_size "$d")))
  done

  if [ ! -f "$llamacpp_bin" ]; then
    echo "::error::payload_check llama_server: MISSING — ${llamacpp_bin} 不存在。llama.cpp 交叉编译产物未产出，或 tarball 分支未命中。缺 llama-server ⇒ 端模型对话完全不可用。"
    n_fail=$((n_fail + 1))
    llamacpp_rep="MISSING"
  elif [ "$llamacpp_total" -lt "$MIN_LLAMACPP_BYTES" ]; then
    echo "::error::payload_check llama_server: TOO SMALL — 主二进制 ${llamacpp_sz} B + dylib ${llamacpp_libs} B = 合计 ${llamacpp_total} B（$(human "$llamacpp_total")）低于下限 ${MIN_LLAMACPP_BYTES} B（$(human "$MIN_LLAMACPP_BYTES")）。疑似下到 HTML 错误页或文件被截断（合法 SHARED 构建应同目录带 libllama/libcommon dylib）。"
    n_fail=$((n_fail + 1))
    llamacpp_rep="CORRUPT($(human "$llamacpp_total"))"
  else
    llamacpp_rep="$(human "$llamacpp_total")$([ "$llamacpp_libs" -gt 0 ] && echo "+libs($(human "$llamacpp_libs"))")"
  fi

  # =========================================================================
  # C2-b  PostgreSQL —— 四个 sidecar + lib + share，缺失硬失败
  # =========================================================================
  local pg_total=0 t tp tsz missing_pg=""
  echo "--- postgresql ---"
  for t in postgres initdb pg_ctl psql; do
    tp="${BUNDLE}/bin/${t}-${TARGET_TRIPLE}"
    tsz="$(file_size "$tp")"
    if [ ! -f "$tp" ]; then
      missing_pg="${missing_pg:+$missing_pg,}${t}"
    elif [ "$tsz" -lt "$MIN_PGTOOL_BYTES" ]; then
      missing_pg="${missing_pg:+$missing_pg,}${t}(too-small:${tsz}B)"
    else
      pg_total=$((pg_total + tsz))
    fi
    ls -lh "$tp" 2>/dev/null || echo "  (缺失: ${t})"
  done

  local pg_lib_sz pg_share_sz pg_rep
  # llama-server 为单二进制，lib/ 下只有 postgres 依赖，无需排除 runner 子目录。
  pg_lib_sz="$(dir_bytes "${BUNDLE}/lib")"
  pg_share_sz="$(dir_bytes "${BUNDLE}/share")"
  echo "  lib = $(human "$pg_lib_sz")   share = $(human "$pg_share_sz")"

  if [ -n "$missing_pg" ]; then
    echo "::error::payload_check postgres: MISSING/BAD sidecars = [${missing_pg}]。Postgres.app dmg 下载或 7z 解包未成功（该步骤所有失败路径都是 warning+exit 0，因此步骤会假绿）。缺 postgres/initdb ⇒ App 启动即报数据库不可用。"
    n_fail=$((n_fail + 1))
  fi
  if [ "$pg_share_sz" -lt "$MIN_PGSHARE_BYTES" ]; then
    echo "::error::payload_check postgres: share/ 过小 — 实际 $(human "$pg_share_sz") 低于下限 $(human "$MIN_PGSHARE_BYTES")。initdb 依赖 share 里的模板与时区数据，缺失则数据库无法初始化。"
    n_fail=$((n_fail + 1))
  fi
  if [ "$pg_lib_sz" -lt "$MIN_PGLIB_BYTES" ]; then
    echo "::error::payload_check postgres: lib/ 过小 — 实际 $(human "$pg_lib_sz") 低于下限 $(human "$MIN_PGLIB_BYTES")。pg 可执行文件用 @loader_path/../lib 找 dylib，缺失则一启动就 'Library not loaded'。"
    n_fail=$((n_fail + 1))
  fi
  # initdb 没有 postgres.bki 就跑不起来。布局可能随上游变动，故仅告警不阻断。
  if [ -d "${BUNDLE}/share" ] && ! find "${BUNDLE}/share" -name 'postgres.bki' -print -quit 2>/dev/null | grep -q .; then
    echo "::warning::payload_check postgres: 在 share/ 下未找到 postgres.bki（initdb 引导必需）。若用户侧 initdb 失败，优先查这里。"
  fi
  pg_rep="bin$(human "$pg_total")+lib$(human "$pg_lib_sz")+share$(human "$pg_share_sz")"
  [ -n "$missing_pg" ] && pg_rep="MISSING[${missing_pg}]"

  # =========================================================================
  # C2-c  端模型权重 —— best-effort，但必须显式区分 BAKED / SKIPPED
  # =========================================================================
  local model_sz model_rep magic
  model_sz="$(file_size "$MODEL")"
  echo "--- model weights ---"
  ls -lh "$MODEL" 2>/dev/null || echo "  (未烘焙: $MODEL)"

  if [ -z "$MODEL" ] || [ ! -f "$MODEL" ]; then
    model_rep="SKIPPED"
    echo "::warning::payload_check model_weights=SKIPPED — 端模型权重未烘焙进包。这是**设计上允许的降级路径**：用户首次启动若未放置本地 GGUF 权重，llama-server 不会启动，本地 chat 不可用，但在线厂商路径不受影响。"
  else
    magic="$(head -c 4 "$MODEL" 2>/dev/null | tr -d '\0')"
    if [ "$magic" != "GGUF" ]; then
      echo "::error::payload_check model_weights: CORRUPT — 文件存在但魔数不是 GGUF（实际前 4 字节 ='${magic}'）。发一个坏模型比不发更糟，硬失败。"
      n_fail=$((n_fail + 1)); model_rep="CORRUPT"
    elif [ "$model_sz" -lt "$MIN_MODEL_BYTES" ]; then
      echo "::error::payload_check model_weights: TRUNCATED — 实际 ${model_sz} B（$(human "$model_sz")）远低于 qwen2.5-0.5b q4_k_m 的预期量级（约 400MB，下限 $(human "$MIN_MODEL_BYTES")）。"
      n_fail=$((n_fail + 1)); model_rep="TRUNCATED($(human "$model_sz"))"
    else
      model_rep="BAKED($(human "$model_sz"))"
    fi
  fi

  # =========================================================================
  # 汇总注解：无条件打印，下一轮可直接从注解读事实，不必猜
  # =========================================================================
  local total
  total=$(( $(dir_bytes "$BUNDLE") + model_sz ))
  echo "::notice::payload_check llama_server=${llamacpp_rep} postgres=${pg_rep} model_weights=${model_rep} total=$(human "$total")"

  if [ "$n_fail" -ne 0 ]; then
    echo "::error::payload_check: 共 ${n_fail} 条断言失败 —— 在昂贵的交叉编译之前中止，避免白烧一轮。"
    return 1
  fi
  echo "payload_check: 全部断言通过"
  return 0
}

# =============================================================================
# 自测：造一棵假的 .bundle 树，覆盖「齐全 / 缺 llama-server / 缺 pg / 模型损坏」形态
# =============================================================================
self_test() {
  local pass=0 fail=0 tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  _mk() {  # $1=path $2=KiB
    mkdir -p "$(dirname "$1")"
    dd if=/dev/zero of="$1" bs=1024 count="$2" >/dev/null 2>&1
  }
  _expect() {
    if [ "$2" = "$3" ]; then printf '  [PASS] %-42s => rc=%s\n' "$1" "$3"; pass=$((pass+1))
    else printf '  [FAIL] %-42s expected rc=%s actual rc=%s\n' "$1" "$2" "$3"; fail=$((fail+1)); fi
  }

  # --- 形态 1：一切齐全 ---
  local B="$tmp/full"
  _mk "$B/bin/llama-server-${TARGET_TRIPLE}" 10240          # 10 MiB
  _mk "$B/bin/postgres-${TARGET_TRIPLE}" 512
  _mk "$B/bin/initdb-${TARGET_TRIPLE}" 256
  _mk "$B/bin/pg_ctl-${TARGET_TRIPLE}" 256
  _mk "$B/bin/psql-${TARGET_TRIPLE}" 256
  _mk "$B/lib/libpq.5.dylib" 2048
  _mk "$B/share/postgresql/postgres.bki" 2048
  local M="$tmp/model.gguf"
  printf 'GGUF' > "$M"; dd if=/dev/zero bs=1024 count=70000 >> "$M" 2>/dev/null   # ~68 MiB
  main "$B" "$M" >/dev/null 2>&1; _expect "全部齐全" 0 "$?"

  # --- 形态 2：缺 llama-server（硬失败）---
  local B2="$tmp/nollamacpp"; cp -a "$B" "$B2"; rm -f "$B2/bin/llama-server-${TARGET_TRIPLE}"
  main "$B2" "$M" >/dev/null 2>&1; _expect "缺 llama-server ⇒ 硬失败" 1 "$?"

  # --- 形态 3：缺 postgres sidecar（硬失败）---
  local B3="$tmp/nopg"; cp -a "$B" "$B3"; rm -f "$B3/bin/initdb-${TARGET_TRIPLE}"
  main "$B3" "$M" >/dev/null 2>&1; _expect "缺 initdb ⇒ 硬失败" 1 "$?"

  # --- 形态 4：pg share 为空（硬失败）---
  local B4="$tmp/noshare"; cp -a "$B" "$B4"; rm -rf "$B4/share"; mkdir -p "$B4/share"
  main "$B4" "$M" >/dev/null 2>&1; _expect "pg share 空 ⇒ 硬失败" 1 "$?"

  # --- 形态 5：模型缺失（best-effort，仍应通过）---
  main "$B" "$tmp/nonexistent.gguf" >/dev/null 2>&1; _expect "模型缺失 ⇒ best-effort 放行" 0 "$?"
  # SKIPPED 会出现两次（解释性 ::warning:: + 汇总 ::notice::），这里只校验汇总注解
  local out
  out="$(main "$B" "$tmp/nonexistent.gguf" 2>&1 | grep -c '^::notice::payload_check .*model_weights=SKIPPED')"
  _expect "汇总注解含 model_weights=SKIPPED" 1 "$out"
  out="$(main "$B" "$tmp/nonexistent.gguf" 2>&1 | grep -c '^::warning::payload_check model_weights=SKIPPED')"
  _expect "另有 warning 解释降级后果" 1 "$out"

  # --- 形态 6：模型魔数错（硬失败）---
  local BAD="$tmp/bad.gguf"; printf '<!DOCTYPE html>' > "$BAD"
  dd if=/dev/zero bs=1024 count=70000 >> "$BAD" 2>/dev/null
  main "$B" "$BAD" >/dev/null 2>&1; _expect "模型魔数错 ⇒ 硬失败" 1 "$?"

  # --- 形态 7：模型截断（硬失败）---
  local TR="$tmp/trunc.gguf"; printf 'GGUF' > "$TR"; dd if=/dev/zero bs=1024 count=100 >> "$TR" 2>/dev/null
  main "$B" "$TR" >/dev/null 2>&1; _expect "模型截断 ⇒ 硬失败" 1 "$?"

  # --- 形态 8：BAKED 注解正确 ---
  out="$(main "$B" "$M" 2>&1 | grep -c '^::notice::payload_check .*model_weights=BAKED')"
  _expect "汇总注解含 model_weights=BAKED" 1 "$out"

  # --- 形态 9：llama_server 注解正确（单二进制，无 runner 三态）---
  out="$(main "$B" "$M" 2>&1 | grep -c '^::notice::payload_check .*llama_server=')"
  _expect "汇总注解含 llama_server=" 1 "$out"
  out="$(main "$B2" "$M" 2>&1 | grep -c '^::notice::payload_check llama_server=MISSING ')"
  _expect "llama_server=MISSING 注解正确" 1 "$out"

  echo
  echo "self-test: ${pass} passed, ${fail} failed"
  [ "$fail" -eq 0 ]
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi
main "$@"
exit $?
