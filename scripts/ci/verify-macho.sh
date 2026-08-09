#!/usr/bin/env bash
# =============================================================================
# verify-macho.sh — 交叉编译产物的**静态** Mach-O 断言（在 Linux runner 上跑）
#
# 【为什么需要它】
#   run #16 首次全绿，产出 aidea-bundle 586 MiB。但「打包步骤跑完」**不等于**
#   「二进制能在 Intel Mac 上启动」。Linux runner 无法**执行** Mach-O，
#   但完全可以**静态检查**它。本脚本做四类断言：
#
#     B1 架构     : Mach-O magic 合法，且 cputype == x86_64
#                   （用户机是 Intel；若产出 arm64 整个包作废）
#     B2 minos    : LC_BUILD_VERSION.minos == 10.15   ← **本脚本最重要的断言**
#     B3 路径泄漏 : LC_LOAD_DYLIB 不得含构建机本地路径（/osxcross、/home/runner…）
#     B4 非空     : 文件存在且大于阈值（挡住空文件 / 截断 / sidecar 占位脚本）
#
# 【为什么 B2 最重要】
#   链接成功 ≠ deployment target 生效。若 ld64 把 minos 静默提升到 SDK 版本
#   （26.1），dyld 会在用户机上**直接拒绝启动**（"requires macOS 26.1 or
#   later"），而 CI 永远发现不了 —— 用户下载 586 MB、封 dmg、双击、失败，
#   才知道白折腾一场。所以 B2 **硬失败**，绝不降级为 warning。
#
# 【工具探测】
#   优先 osxcross 自带的 cctools otool（x86_64-apple-darwin*-otool），
#   依次回退 llvm-otool → otool → `llvm-objdump --macho --private-headers`。
#   最后那个由 apt 的 `llvm` 包提供（workflow 的 "Install host build
#   dependencies" 已装），因此「一个可用工具都没有」几乎不可能发生。
#   三种工具的**文本输出格式一致**（llvm-otool 本就是 llvm-objdump 的
#   MachO printer 包装），所以下面的 parser 一套通吃，只是 cctools 带 -v 时
#   打符号名（X86_64 / MH_MAGIC_64），llvm 系打数值（16777223 / 0xfeedfacf）
#   —— parser 两种都认。
#
# 用法:
#   verify-macho.sh <expect_arch> <expect_minos> <label>=<path> [<label>=<path> …]
#   verify-macho.sh --self-test        # 用模拟 otool 输出自测 parser
# =============================================================================

set -uo pipefail   # 故意不开 -e：所有断言由脚本自己收敛成最终退出码

# ---- 体积下限（B4）。取值远低于真实体积，只为挡住空文件 / 截断 / 占位脚本，
#      因此不存在「误杀正常产物」的风险。 --------------------------------------
: "${MACHO_MIN_BYTES:=2097152}"   # 2 MiB

# ---- 路径泄漏黑名单（B3，命中即硬失败）--------------------------------------
#      任何指向 Linux 构建机的 LC_LOAD_DYLIB 都会让二进制在 Mac 上找不到库。
LEAK_RE='(^|/)osxcross(/|$)|^/home/|^/github/|^/builds/|^/__w/|^/tmp/|^/opt/|^/root/|^/mnt/|^/var/lib/'

# ---- 已知合法前缀（不在此列的记 warning，但不阻断）---------------------------
SANE_RE='^/usr/lib/|^/System/Library/|^/Library/Frameworks/|^@rpath/|^@loader_path/|^@executable_path/'

# -----------------------------------------------------------------------------
# mv_parse — 从 (otool -h -v && otool -l) 或 (llvm-objdump --macho
#            --private-headers) 的文本里抽取 magic / cpu / platform / minos / sdk。
#
# 解析要点：
#   * 用 `$1 == "cmd"` 而不是正则来切 load command 边界 —— Ubuntu 默认 awk 是
#     mawk，避开 \t / POSIX 字符类的方言差异，最稳。
#   * LC_BUILD_VERSION 块内 sdk 之后还有 `ntools/tool/version`，其中的
#     `version 1053.12` 是**工具链版本**不是 minos；因为它落在 bv 块而非 vm 块，
#     `vm && $1=="version"` 不会误取。
#   * 10.14 以前的产物用 LC_VERSION_MIN_MACOSX（字段名是 `version` 不是
#     `minos`），一并兼容并在 lc= 里标明来源。
# -----------------------------------------------------------------------------
mv_parse() {
  awk '
    /^Mach header/                               { inhdr = 1; next }
    inhdr && /magic/ && /cputype/                { want = 1; next }
    want                                         { magic = $1; cpu = $2; want = 0; inhdr = 0; next }

    $1 == "cmd" && $2 == "LC_BUILD_VERSION"      { bv = 1; vm = 0; next }
    $1 == "cmd" && $2 == "LC_VERSION_MIN_MACOSX" { vm = 1; bv = 0; next }
    $1 == "cmd"                                  { bv = 0; vm = 0; next }

    bv && $1 == "platform" && plat == "" { plat = $2 }
    bv && $1 == "minos"    && bmin == "" { bmin = $2 }
    bv && $1 == "sdk"      && bsdk == "" { bsdk = $2 }
    vm && $1 == "version"  && vmin == "" { vmin = $2 }
    vm && $1 == "sdk"      && vsdk == "" { vsdk = $2 }

    END {
      minos = (bmin != "" ? bmin : vmin)
      sdk   = (bsdk != "" ? bsdk : vsdk)
      lc    = (bmin != "" ? "LC_BUILD_VERSION" \
                          : (vmin != "" ? "LC_VERSION_MIN_MACOSX" : "NONE"))
      printf "magic=%s\n",    (magic == "" ? "?" : magic)
      printf "cpu=%s\n",      (cpu   == "" ? "?" : cpu)
      printf "platform=%s\n", (plat  == "" ? "?" : plat)
      printf "minos=%s\n",    (minos == "" ? "?" : minos)
      printf "sdk=%s\n",      (sdk   == "" ? "?" : sdk)
      printf "lc=%s\n",       lc
    }
  '
}

# mv_dylibs — 从 `otool -L` / `--dylibs-used` 输出里抽出依赖路径（每行一个）。
# 依赖行的判定：含 "(compatibility version"，比「以 tab 开头」更抗格式漂移。
mv_dylibs() {
  awk '/\(compatibility version/ { print $1 }'
}

norm_cpu() {
  case "${1:-}" in
    X86_64|x86_64|16777223|0x01000007)              echo "x86_64" ;;
    ARM64|arm64|16777228|0x0100000c|0x0100000C)     echo "arm64" ;;
    ARM64E|arm64e)                                  echo "arm64e" ;;
    I386|i386|7)                                    echo "i386" ;;
    *)                                              echo "unknown(${1:-empty})" ;;
  esac
}

# 10.15 / 10.15.0 视为同一个版本；26.1 保持 26.1
norm_ver() { printf '%s' "${1:-}" | sed -E 's/(\.0)+$//'; }

magic_ok() {
  case "${1:-}" in
    MH_MAGIC_64|0xfeedfacf) return 0 ;;
    *)                      return 1 ;;
  esac
}

file_size() { wc -c < "$1" 2>/dev/null | tr -d ' []' ; }

human() {  # bytes -> "45.2MB"
  awk -v b="${1:-0}" 'BEGIN{ printf "%.1fMB", b/1048576 }'
}

# =============================================================================
# 自测：用模拟 otool 输出验证 parser。**这是本轮最关键的断言，解析错了等于没做**，
# 所以在 CI 真跑之前先在本地把六种典型输入喂进去对答案。
# =============================================================================
self_test() {
  local pass=0 fail=0
  _expect() {  # $1=case  $2=expected  $3=actual
    if [ "$2" = "$3" ]; then
      printf '  [PASS] %-46s => %s\n' "$1" "$3"; pass=$((pass + 1))
    else
      printf '  [FAIL] %-46s expected=%s actual=%s\n' "$1" "$2" "$3"; fail=$((fail + 1))
    fi
  }
  _get() { printf '%s\n' "$1" | mv_parse | grep "^$2=" | cut -d= -f2- ; }

  # --- 1) cctools otool -h -v + -l，正确的 10.15 产物（期望形态）-------------
  local CCTOOLS_GOOD
  CCTOOLS_GOOD='dist-bundle/payload/macos/aidea-gui:
Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE    27       4128   NOUNDEFS DYLDLINK TWOLEVEL PIE
Load command 0
      cmd LC_SEGMENT_64
  cmdsize 72
  segname __PAGEZERO
Load command 9
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 10.15
      sdk 26.1
   ntools 1
     tool 3
  version 1053.12
Load command 10
      cmd LC_SOURCE_VERSION
  cmdsize 16
  version 0.0'
  _expect "cctools/good: cpu"      "X86_64"           "$(_get "$CCTOOLS_GOOD" cpu)"
  _expect "cctools/good: minos"    "10.15"            "$(_get "$CCTOOLS_GOOD" minos)"
  _expect "cctools/good: sdk"      "26.1"             "$(_get "$CCTOOLS_GOOD" sdk)"
  _expect "cctools/good: lc"       "LC_BUILD_VERSION" "$(_get "$CCTOOLS_GOOD" lc)"
  _expect "cctools/good: magic"    "MH_MAGIC_64"      "$(_get "$CCTOOLS_GOOD" magic)"
  # LC_SOURCE_VERSION 的 `version 0.0` 与 LC_BUILD_VERSION 的 `version 1053.12`
  # 都不得污染 minos —— 上面 minos=10.15 已经覆盖该回归点。

  # --- 2) llvm-objdump 数值形态 ---------------------------------------------
  local LLVM_GOOD
  LLVM_GOOD='dist-bundle/payload/bin/aidea-x86_64-apple-darwin:
Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
 0xfeedfacf 16777223          3  0x00           2    25       3456   0x00200085
Load command 8
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 10.15
      sdk 26.1
   ntools 1'
  _expect "llvm/good: cpu(numeric)"  "16777223" "$(_get "$LLVM_GOOD" cpu)"
  _expect "llvm/good: minos"         "10.15"    "$(_get "$LLVM_GOOD" minos)"
  _expect "llvm/good: magic"         "0xfeedfacf" "$(_get "$LLVM_GOOD" magic)"
  _expect "llvm/good: norm_cpu"      "x86_64"   "$(norm_cpu "$(_get "$LLVM_GOOD" cpu)")"

  # --- 3) ★核心回归：ld64 把 minos 悄悄提到 26.1（必须被抓到）---------------
  local BAD_MINOS
  BAD_MINOS='Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE    27       4128   NOUNDEFS
Load command 9
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 26.1
      sdk 26.1
   ntools 1'
  _expect "BAD minos 26.1 被正确提取"  "26.1"   "$(_get "$BAD_MINOS" minos)"
  local m; m="$(norm_ver "$(_get "$BAD_MINOS" minos)")"
  _expect "BAD minos != 10.15 (会硬失败)" "MISMATCH" \
          "$([ "$m" = "10.15" ] && echo MATCH || echo MISMATCH)"

  # --- 4) ★核心回归：arm64 产物必须被抓到 -----------------------------------
  local ARM
  ARM='Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
 0xfeedfacf 16777228          0  0x00           2    25       3456   0x00200085
Load command 9
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 10.15
      sdk 26.1'
  _expect "arm64 被识别"  "arm64"  "$(norm_cpu "$(_get "$ARM" cpu)")"

  # --- 5) 旧格式 LC_VERSION_MIN_MACOSX（字段名是 version 不是 minos）--------
  local LEGACY
  LEGACY='Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE    20       2400   NOUNDEFS
Load command 7
      cmd LC_VERSION_MIN_MACOSX
  cmdsize 16
  version 10.13
      sdk 26.1'
  _expect "legacy: minos 取自 version"  "10.13" "$(_get "$LEGACY" minos)"
  _expect "legacy: lc 标注来源"  "LC_VERSION_MIN_MACOSX" "$(_get "$LEGACY" lc)"

  # --- 6) 完全没有版本 load command（无法验证 ⇒ 必须报 NONE 而不是静默通过）--
  local NOVER
  NOVER='Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE     5        400   NOUNDEFS
Load command 0
      cmd LC_SEGMENT_64
  cmdsize 72'
  _expect "无版本 LC: minos=?"  "?"    "$(_get "$NOVER" minos)"
  _expect "无版本 LC: lc=NONE"  "NONE" "$(_get "$NOVER" lc)"

  # --- 7) 版本号归一化 -------------------------------------------------------
  _expect "norm_ver 10.15.0 -> 10.15" "10.15" "$(norm_ver '10.15.0')"
  _expect "norm_ver 10.15   -> 10.15" "10.15" "$(norm_ver '10.15')"
  _expect "norm_ver 26.1    -> 26.1"  "26.1"  "$(norm_ver '26.1')"

  # --- 8) B3 路径泄漏检测 ----------------------------------------------------
  local LEAKY
  LEAKY='dist-bundle/payload/macos/aidea-gui:
	/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1345.100.2)
	/System/Library/Frameworks/WebKit.framework/Versions/A/WebKit (compatibility version 1.0.0, current version 622.1.15)
	@rpath/libaidea_core.dylib (compatibility version 0.0.0, current version 0.0.0)
	/osxcross/SDK/MacOSX26.1.sdk/usr/lib/libfoo.dylib (compatibility version 1.0.0, current version 1.0.0)
	/home/runner/work/ide/ide/libbar.dylib (compatibility version 1.0.0, current version 1.0.0)'
  local n_dylib leaks
  n_dylib="$(printf '%s\n' "$LEAKY" | mv_dylibs | wc -l | tr -d ' ')"
  leaks="$(printf '%s\n' "$LEAKY" | mv_dylibs | grep -E "$LEAK_RE" | wc -l | tr -d ' ')"
  _expect "leak: 依赖条数"      "5" "$n_dylib"
  _expect "leak: 命中黑名单数"  "2" "$leaks"
  local clean
  clean="$(printf '%s\n' "$LEAKY" | mv_dylibs | grep -Ev "$LEAK_RE" | grep -Evc "$SANE_RE" || true)"
  _expect "leak: 白名单外且非泄漏 = 0" "0" "$clean"

  echo
  echo "self-test: ${pass} passed, ${fail} failed"
  [ "$fail" -eq 0 ]
}

# =============================================================================
# 工具探测：跑通即用。用「能否在真实目标文件上产出 'Mach header'」作为判据，
# 比 `--version` 之类的探测靠谱（cctools otool 根本不支持 --version）。
# =============================================================================
OTOOL=""; OBJDUMP=""; MODE=""

dump_headers() {
  if [ "$MODE" = "otool" ]; then
    "$OTOOL" -h -v "$1" 2>/dev/null
    "$OTOOL" -l "$1"    2>/dev/null
  else
    "$OBJDUMP" --macho --private-headers "$1" 2>/dev/null
  fi
}

dump_dylibs() {
  if [ "$MODE" = "otool" ]; then
    "$OTOOL" -L "$1" 2>/dev/null
  else
    "$OBJDUMP" --macho --dylibs-used "$1" 2>/dev/null
  fi
}

detect_tool() {  # $1 = 一个真实的 Mach-O，用来实测工具是否可用
  local probe="$1" c
  local cands=()
  [ -n "${OSXCROSS_PREFIX:-}" ] && cands+=("${OSXCROSS_PREFIX}-otool")
  if [ -n "${OSXCROSS_ROOT:-}" ] && [ -d "${OSXCROSS_ROOT}/bin" ]; then
    while IFS= read -r c; do
      [ -n "$c" ] && cands+=("$c")
    done < <(ls "${OSXCROSS_ROOT}"/bin/*-apple-darwin*-otool 2>/dev/null | sort -V | tail -3)
  fi
  cands+=("llvm-otool" "otool")

  for c in "${cands[@]}"; do
    command -v "$c" >/dev/null 2>&1 || continue
    OTOOL="$c"; MODE="otool"
    if dump_headers "$probe" | grep -q "^Mach header"; then
      echo "macho tool: ${OTOOL} (mode=otool)"; return 0
    fi
  done

  local ocands=()
  while IFS= read -r c; do [ -n "$c" ] && ocands+=("$c"); done < <(
    { command -v llvm-objdump 2>/dev/null
      ls /usr/bin/llvm-objdump-* 2>/dev/null
      ls /usr/lib/llvm-*/bin/llvm-objdump 2>/dev/null; } | sort -Vu
  )
  for c in "${ocands[@]}"; do
    OBJDUMP="$c"; MODE="objdump"
    if dump_headers "$probe" | grep -q "^Mach header"; then
      echo "macho tool: ${OBJDUMP} (mode=objdump)"; return 0
    fi
  done

  OTOOL=""; OBJDUMP=""; MODE=""
  return 1
}

# =============================================================================
# 主流程
# =============================================================================
main() {
  local expect_arch="$1"; shift
  local expect_minos="$1"; shift
  local n_fail=0

  if [ "$#" -eq 0 ]; then
    echo "::error::verify-macho: 未传入任何待检查的二进制"
    return 1
  fi

  # ---- B4 先行：文件必须存在（否则连工具探测都无从谈起）--------------------
  local spec label path first=""
  for spec in "$@"; do
    label="${spec%%=*}"; path="${spec#*=}"
    if [ ! -f "$path" ]; then
      echo "::error::macho_check ${label}: FILE MISSING — 期望路径 ${path} 不存在（打包装配未产出该二进制）"
      n_fail=$((n_fail + 1))
    elif [ -z "$first" ]; then
      first="$path"
    fi
  done
  if [ -z "$first" ]; then
    echo "::error::macho_check: 所有待检查二进制都不存在，无法继续"
    return 1
  fi

  if ! detect_tool "$first"; then
    echo "::error::macho_check: 找不到可用的 Mach-O 解析工具（试过 osxcross otool / llvm-otool / otool / llvm-objdump）。无法验证 cputype 与 minos —— 这两项是产物能否在用户机启动的前提，因此**不允许静默放行**。"
    return 1
  fi

  for spec in "$@"; do
    label="${spec%%=*}"; path="${spec#*=}"
    [ -f "$path" ] || continue

    local hdr size magic cpu plat minos sdk lc arch
    hdr="$(dump_headers "$path")"
    size="$(file_size "$path")"; size="${size:-0}"

    magic="$(printf '%s\n' "$hdr" | mv_parse | grep '^magic='    | cut -d= -f2-)"
    cpu="$(  printf '%s\n' "$hdr" | mv_parse | grep '^cpu='      | cut -d= -f2-)"
    plat="$( printf '%s\n' "$hdr" | mv_parse | grep '^platform=' | cut -d= -f2-)"
    minos="$(printf '%s\n' "$hdr" | mv_parse | grep '^minos='    | cut -d= -f2-)"
    sdk="$(  printf '%s\n' "$hdr" | mv_parse | grep '^sdk='      | cut -d= -f2-)"
    lc="$(   printf '%s\n' "$hdr" | mv_parse | grep '^lc='       | cut -d= -f2-)"
    arch="$(norm_cpu "$cpu")"

    # ---- B3：依赖清单与泄漏 -------------------------------------------------
    local dl n_dylib leak_list odd_list
    dl="$(dump_dylibs "$path")"
    n_dylib="$(printf '%s\n' "$dl" | mv_dylibs | grep -c . || true)"
    leak_list="$(printf '%s\n' "$dl" | mv_dylibs | grep -E "$LEAK_RE" | tr '\n' ',' | sed 's/,$//' || true)"
    odd_list="$( printf '%s\n' "$dl" | mv_dylibs | grep -Ev "$LEAK_RE" | grep -Ev "$SANE_RE" \
                 | tr '\n' ',' | sed 's/,$//' || true)"

    # ---- 无条件打印关键指标（即使全绿也打），下一轮可直接从注解读事实 -------
    echo "::notice::macho_check ${label}: cputype=${arch} minos=${minos} sdk=${sdk} lc=${lc} platform=${plat} dylibs=${n_dylib} leak=${leak_list:-none} size=$(human "$size")"
    echo "--- ${label} (${path}) ---"
    printf '%s\n' "$dl" | head -40 || true

    # ---- B1 架构（硬失败）---------------------------------------------------
    if ! magic_ok "$magic"; then
      echo "::error::macho_check ${label}: B1 FAIL — Mach-O magic 非法：期望 MH_MAGIC_64/0xfeedfacf，实际 ${magic}（${path} 可能不是 64 位 Mach-O）"
      n_fail=$((n_fail + 1))
    fi
    if [ "$arch" != "$expect_arch" ]; then
      echo "::error::macho_check ${label}: B1 FAIL — cputype 期望 ${expect_arch} 实际 ${arch}（raw=${cpu}）。用户机是 Intel x86_64，架构不符则整个包作废。"
      n_fail=$((n_fail + 1))
    fi

    # ---- B2 minos（★最重要，硬失败）----------------------------------------
    if [ "$lc" = "NONE" ] || [ "$minos" = "?" ]; then
      echo "::error::macho_check ${label}: B2 FAIL — 二进制里既没有 LC_BUILD_VERSION 也没有 LC_VERSION_MIN_MACOSX，无法确认 deployment target。不允许静默放行。"
      n_fail=$((n_fail + 1))
    elif [ "$(norm_ver "$minos")" != "$(norm_ver "$expect_minos")" ]; then
      echo "::error::macho_check ${label}: B2 FAIL — minos 期望 ${expect_minos} 实际 ${minos}（来源 ${lc}，sdk=${sdk}）。deployment target 未按 MACOSX_DEPLOYMENT_TARGET 生效：若实际值高于期望值，该二进制会被用户机的 dyld 直接拒绝启动（\"requires macOS ${minos} or later\"）。这正是「链接成功但产物不可用」的典型形态，必须在发包前拦下。"
      n_fail=$((n_fail + 1))
    fi

    # ---- B3 路径泄漏（黑名单硬失败；白名单外仅告警）-------------------------
    if [ -n "$leak_list" ]; then
      echo "::error::macho_check ${label}: B3 FAIL — LC_LOAD_DYLIB 含构建机本地路径：${leak_list}。这些路径在用户 Mac 上不存在，dyld 会报 'Library not loaded' 而启动失败。"
      n_fail=$((n_fail + 1))
    fi
    if [ -n "$odd_list" ]; then
      echo "::warning::macho_check ${label}: B3 WARN — 依赖路径不在已知安全前缀内（非致命，仅供核对）：${odd_list}"
    fi

    # ---- B4 非空 / 未截断（硬失败）------------------------------------------
    if [ "$size" -lt "$MACHO_MIN_BYTES" ]; then
      echo "::error::macho_check ${label}: B4 FAIL — 体积 ${size} B（$(human "$size")）低于下限 ${MACHO_MIN_BYTES} B（$(human "$MACHO_MIN_BYTES")）。疑似空文件 / 截断 / 误把 sidecar 占位脚本打进包。"
      n_fail=$((n_fail + 1))
    fi
  done

  if [ "$n_fail" -ne 0 ]; then
    echo "::error::macho_check: 共 ${n_fail} 条断言失败 —— 产物不可交付，已阻止上传 artifact。"
    return 1
  fi
  echo "macho_check: 全部断言通过（arch=${expect_arch}, minos=${expect_minos}）"
  return 0
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi
main "$@"
exit $?
