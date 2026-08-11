#!/usr/bin/env bash
# =============================================================================
# verify-macho.sh — 交叉编译产物 + 上游 sidecar 的**静态** Mach-O 断言
#                   （在 Linux runner 上跑，不执行任何 Mach-O）
#
# 【两种模式】
#   strict  （默认，主二进制 aidea / aidea-gui）
#       B1 架构   : magic 合法 且 cputype == <expect_arch>          → 硬失败
#       B2 minos  : == <expect_minos>                               → 硬失败
#       B3 泄漏   : LC_LOAD_DYLIB 不得含构建机本地路径              → 硬失败
#       B4 体积   : >= MACHO_MIN_BYTES (2 MiB)                      → 硬失败
#
#   --sidecar（run #18 新增，上游下载件 ollama / postgres / initdb / pg_ctl / psql）
#       B1 架构   : 必须**含 x86_64 slice**（thin x86_64 或 universal）→ 硬失败
#       B2 minos  : **只打印、不 gate**。上游用什么 deployment target 我们管不
#                   了，拿 10.15 去 gate 会误杀；但若 minos 高于我们的目标，
#                   补一条 ::warning::（对老系统用户是真实风险，仅提示不阻断）。
#       B3 泄漏   : 只打印 + ::warning::，**不 gate**。黑名单是为「我们的 Linux
#                   构建机路径」设计的，上游二进制不可能命中；真命中也多半是
#                   /opt/homebrew 这类我们无权修复的上游问题，不该在此翻红。
#       B4 体积   : >= SIDECAR_MIN_BYTES (16 KiB) → 硬失败。pg 工具本就小
#                   （psql ~1MB、initdb 更小），2 MiB 下限会误杀；16 KiB 只用来
#                   挡空文件 / 占位脚本。分项体积下限由 verify-payload.sh 负责。
#
# 【为什么 run #18 必须加 sidecar 模式】
#   run #17 只验了 aidea + aidea-gui（合计 17.4MB，占包体不到 3%）。包里另外
#   5 个同样要在用户 Intel Mac 上执行的 Mach-O 全部来自上游下载，架构从未检查。
#   若 Postgres.app / ollama 是 arm64-only，用户机上数据库或推理**启动即失败**，
#   而当时所有断言照样全绿 —— 占包体 95% 以上的部分处于完全无监控状态。
#
# 【★ fat (universal) 二进制处理 —— 本脚本最容易翻车的地方】
#   上游分发的 macOS 二进制大概率是 universal（arm64 + x86_64 两个 slice）。
#   原有 parser 是按**单架构 thin** 写的，直接喂 fat 文件会错得很隐蔽：
#
#     ① `otool -h` 对 fat 文件**不打印所有 slice**。cctools 的 ofile_process()
#        在未给 -arch 时会优先只处理**宿主架构**：Linux runner 是 x86_64，
#        于是 fat 文件只打印 x86_64 那片 —— 看起来"正常"，但这是巧合，
#        换个宿主就变。若 fat 里没有 x86_64，它才回退成打印全部 slice，
#        且每段前面多一行 `file (architecture arm64e):`，格式完全不同。
#     ② llvm-objdump / llvm-otool 对 fat 文件**默认打印全部 slice**，
#        顺序由文件内的 fat_arch 数组决定。若 arm64 排在前面，
#        "取第一个 Mach header" 就会读到 arm64 → **假失败**。
#
#   因此本脚本**不依赖任何工具的默认行为**：
#     · thin/fat 判定 + slice 枚举：直接用 `od` 读文件头字节，自己解析
#       fat_header/fat_arch（大端）。零工具依赖、完全确定、可用纯 hex 自测。
#     · 拿到 slice 列表后，用 `otool -arch <slice>` 精确取该 slice 的 header；
#       若工具不支持 -arch，回退到「-arch all 输出 + 按 (architecture X): 切段」；
#       再不行才回退默认输出。三级降级，任何一级都不会读错 slice。
#
# 【工具探测】osxcross cctools otool → llvm-otool → otool → llvm-objdump。
#   四者的文本格式一致，parser 一套通吃（cctools -v 打符号名 X86_64/MH_MAGIC_64，
#   llvm 打数值 16777223/0xfeedfacf，两种都认）。
#
# 用法:
#   verify-macho.sh [--sidecar] <expect_arch> <expect_minos> <label>=<path> …
#   verify-macho.sh --self-test        # 纯模拟数据自测 parser，可离线跑
# =============================================================================

set -uo pipefail   # 故意不开 -e：所有断言由脚本自己收敛成最终退出码

# ---- 体积下限。取值远低于真实体积，只为挡住空文件 / 截断 / 占位脚本。--------
: "${MACHO_MIN_BYTES:=2097152}"    # 2 MiB —— strict 模式（我们自己编的两个）
: "${SIDECAR_MIN_BYTES:=16384}"    # 16 KiB —— sidecar 模式（pg 工具本就小）
: "${BUNDLE_MIN_MACOS:=11.0}"      # sidecar B2 比较基准：本包声明的真实最低系统
                                   # （打过 minos 补丁后，ollama 真实下限=11.0，不再与 10.15 比）

# ---- 路径泄漏黑名单（B3）-----------------------------------------------------
LEAK_RE='(^|/)osxcross(/|$)|^/home/|^/github/|^/builds/|^/__w/|^/tmp/|^/opt/|^/root/|^/mnt/|^/var/lib/'

# ---- 已知合法前缀（不在此列的记 warning，但不阻断）---------------------------
SANE_RE='^/usr/lib/|^/System/Library/|^/Library/Frameworks/|^@rpath/|^@loader_path/|^@executable_path/'

# =============================================================================
# 第一部分：fat / thin 判定与 slice 枚举（纯字节解析，不依赖任何 Mach-O 工具）
# =============================================================================
#
# fat_header（大端，位于文件最开头）:
#     uint32 magic        cafebabe = FAT_MAGIC / cafebabf = FAT_MAGIC_64
#     uint32 nfat_arch
# 其后是 nfat_arch 个 fat_arch：
#     FAT_MAGIC   : cputype, cpusubtype, offset, size, align            = 20 B
#     FAT_MAGIC_64: cputype, cpusubtype, offset(8), size(8), align, rsv = 32 B
#
# 注意：magic 一律按**字节序列**比对（Apple 永远大端写 fat header），
#       所以不存在 FAT_CIGAM 的字节序歧义。
# -----------------------------------------------------------------------------

# fat_parse_hex — stdin 收「文件头的连续 hex 字符串」，stdout 输出每片一行：
#                 "<cputype_hex> <cpusubtype_hex>"；thin 文件输出空。
# 单独抽成函数是为了能用纯 hex 字面量自测，不必造真文件。
fat_parse_hex() {
  awk '
    function h2d(s,   i, c, v, r) {
      r = 0
      for (i = 1; i <= length(s); i++) {
        c = tolower(substr(s, i, 1))
        v = index("0123456789abcdef", c) - 1
        if (v < 0) return -1
        r = r * 16 + v
      }
      return r
    }
    {
      hex   = tolower($0)
      magic = substr(hex, 1, 8)
      # 只认这两个 fat magic。thin Mach-O 是 cffaedfe / feedfacf，直接退出。
      if (magic != "cafebabe" && magic != "cafebabf") exit 0
      n = h2d(substr(hex, 9, 8))
      if (n < 1 || n > 32) exit 0
      esz = (magic == "cafebabf") ? 32 : 20     # 每个 fat_arch 的字节数
      if (17 + 15 > length(hex)) exit 0

      # ★ 反误判：Java .class 也以 cafebabe 开头，其 bytes[4..7] 是
      #   minor/major version（如 0x00000034 = Java 8），恰好落在 nfat_arch
      #   的"合理"区间里，光靠 n 的范围挡不住。真正可靠的判据是**第一片的
      #   cputype 必须是已知的 Mach-O cputype**；.class 那里是常量池内容，
      #   几乎不可能命中白名单。命中不了就当作 thin/非 Mach-O 处理。
      known = "|01000007|0100000c|0200000c|00000007|0000000c|01000012|00000012|0000000a|"
      if (index(known, "|" substr(hex, 17, 8) "|") == 0) exit 0

      for (i = 0; i < n; i++) {
        off = 17 + i * esz * 2                  # hex 串里的 1-based 起始下标
        if (off + 15 > length(hex)) break       # 头部读得不够，停止（不报错）
        printf "%s %s\n", substr(hex, off, 8), substr(hex, off + 8, 8)
      }
    }
  '
}

# fat_slices — $1=path；输出每片一行的**规范化架构名**（x86_64 / arm64 / arm64e …）。
#              thin 文件输出空 —— 调用方以「输出是否为空」判定 fat/thin。
fat_slices() {
  od -An -v -tx1 -N 8192 "$1" 2>/dev/null | tr -d ' \n' | fat_parse_hex | while read -r ct cs; do
    norm_cpu_hex "$ct" "$cs"
  done
}

# norm_cpu_hex — $1=cputype hex(8) $2=cpusubtype hex(8) → 架构名
# cpusubtype 高位是 capability bits（如 arm64e 的 PTR_AUTH），取低 24 位。
norm_cpu_hex() {
  local ct="${1:-}" cs="${2:-}" sub
  sub="$(awk -v h="$cs" 'BEGIN{
    r = 0
    for (i = 1; i <= length(h); i++) {
      c = tolower(substr(h, i, 1)); v = index("0123456789abcdef", c) - 1
      if (v < 0) { print "0"; exit }
      r = r * 16 + v
    }
    print r % 16777216
  }')"
  case "$ct" in
    01000007) case "$sub" in 8) echo "x86_64h" ;; *) echo "x86_64" ;; esac ;;
    0100000c) case "$sub" in 2) echo "arm64e" ;; *) echo "arm64" ;; esac ;;
    00000007) echo "i386" ;;
    0000000c) echo "arm" ;;
    01000012) echo "ppc64" ;;
    00000012) echo "ppc" ;;
    *)        echo "unknown(0x${ct})" ;;
  esac
}

# has_x86 — 判断 slice 列表（每行一个）里是否有 x86_64 系（含 x86_64h 子型）。
has_x86() { printf '%s\n' "$1" | grep -qE '^x86_64(h)?$'; }

# arch_desc — 组装人类可读的形态串：
#   universal(x86_64+arm64) / x86_64(thin) / arm64(thin)❌ / universal(arm64+arm64e)❌
# $1=slices（多行，空表示 thin） $2=thin 架构名 $3=是否满足 x86_64 要求(0/1)
arch_desc() {
  local slices="$1" thin_arch="$2" ok="$3" joined desc
  if [ -z "$slices" ]; then
    desc="${thin_arch}(thin)"
  else
    joined="$(printf '%s\n' "$slices" | paste -sd '+' - 2>/dev/null \
              || printf '%s\n' "$slices" | tr '\n' '+' | sed 's/+$//')"
    desc="universal(${joined})"
  fi
  [ "$ok" = "1" ] || desc="${desc}❌"
  printf '%s' "$desc"
}

# =============================================================================
# 第二部分：otool / objdump 文本解析
# =============================================================================

# mv_arch_section — 从**多架构**dump 文本里切出指定 slice 的那一段。
#   分隔行形如 `dist/bin/ollama (architecture x86_64):`，cctools 与 llvm 一致。
#   若文本里根本没有分隔行（单架构 dump），则原样透传。
#   ★ 这是防「arm64 排在前面导致读错 slice」的关键，见文件头 ② 。
mv_arch_section() {
  awk -v want="$1" '
    {
      p = index($0, "(architecture ")
      # 行尾必须是 "):"，避免误伤正文里恰好含该子串的行
      if (p > 0 && substr($0, length($0) - 1, 2) == "):") {
        cur = substr($0, p + 14)
        sub(/\):$/, "", cur)
        on   = (cur == want)
        seen = 1
        next
      }
      if (seen == 0 || on == 1) print
    }
  '
}

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
mv_parse() {
  awk '
    /^Mach header/                               { inhdr = 1; next }
    inhdr && /magic/ && /cputype/                { want = 1; next }
    # ★ first-wins：多架构 dump 里会出现多个 Mach header。无条件赋值会变成
    #   "最后一片胜出"，取到哪一片全看文件里 slice 的排列顺序 —— 不确定行为。
    #   这里与下面 minos/sdk/platform 的 `== ""` 守卫保持一致，统一取第一片，
    #   使「未切段」时的结果至少是确定的。正确做法仍是先用 mv_arch_section
    #   把目标 slice 切出来再喂进来，本守卫只是兜底防止读到随机 slice。
    want                                         { if (magic == "") { magic = $1; cpu = $2 }
                                                   want = 0; inhdr = 0; next }

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
    X86_64|x86_64|CPU_TYPE_X86_64|16777223|0x01000007)          echo "x86_64" ;;
    ARM64|arm64|CPU_TYPE_ARM64|16777228|0x0100000c|0x0100000C)  echo "arm64" ;;
    ARM64E|arm64e)                                              echo "arm64e" ;;
    I386|i386|CPU_TYPE_I386|7)                                  echo "i386" ;;
    *)                                                          echo "unknown(${1:-empty})" ;;
  esac
}

# 10.15 / 10.15.0 视为同一个版本；26.1 保持 26.1
norm_ver() { printf '%s' "${1:-}" | sed -E 's/(\.0)+$//'; }

# ver_gt — 数值比较 "$1 > $2"（按点分段逐段比整数）。
#   必须数值比较：字符串比较会把 10.15 判成小于 10.9，把 14.0 判成大于 10.15 纯属巧合。
ver_gt() {
  awk -v a="${1:-0}" -v b="${2:-0}" 'BEGIN{
    na = split(a, A, "."); nb = split(b, B, ".")
    n  = (na > nb ? na : nb)
    for (i = 1; i <= n; i++) {
      x = (i <= na ? A[i] + 0 : 0); y = (i <= nb ? B[i] + 0 : 0)
      if (x > y) { print "1"; exit }
      if (x < y) { print "0"; exit }
    }
    print "0"
  }' | grep -q '^1$'
}

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
# 第三部分：自测 —— 用模拟数据验证 parser。
# **解析错了等于没做**，所以在 CI 真跑之前先把所有典型输入喂进去对答案。
# 全部使用字面量，不依赖任何外部文件/工具，Linux 与 macOS 上结果一致。
# =============================================================================
self_test() {
  local pass=0 fail=0
  _expect() {  # $1=case  $2=expected  $3=actual
    if [ "$2" = "$3" ]; then
      printf '  [PASS] %-52s => %s\n' "$1" "$3"; pass=$((pass + 1))
    else
      printf '  [FAIL] %-52s expected=%s actual=%s\n' "$1" "$2" "$3"; fail=$((fail + 1))
    fi
  }
  _get() { printf '%s\n' "$1" | mv_parse | grep "^$2=" | cut -d= -f2- ; }
  # 把 fat_parse_hex 的输出（cputype/cpusubtype hex）再过一遍 norm_cpu_hex
  _slices() { printf '%s\n' "$1" | fat_parse_hex | while read -r a b; do norm_cpu_hex "$a" "$b"; done | tr '\n' ' ' | sed 's/ $//'; }

  echo "===== A. fat header 字节解析（thin/fat 判定 + slice 枚举）====="

  # ── A1) 真·universal：x86_64 + arm64。
  #        cputype/cpusubtype/size 取自 ollama v0.32.6 `ollama-darwin.tgz` 里
  #        `ollama` 的**真实 fat 头**（本地 ranged download 实测：
  #         nfat_arch=2 / 01000007 sub=3 size=34,863,936 / 0100000c sub=0
  #         size=31,552,176）。所有 hex 字面量均由 struct.pack('>…') 生成，
  #        不是手数出来的 —— 手数 align 字段正是第一版自测抓到的错误。
  local FAT_UNIV='cafebabe00000002010000070000000300004000021401800000000e0100000c000000000216000001e159b00000000e'
  _expect "A1 universal(x86_64+arm64) 枚举" "x86_64 arm64" "$(_slices "$FAT_UNIV")"
  _expect "A1 含 x86_64 ⇒ 通过"             "YES" \
          "$(has_x86 "$(printf '%s\n' "$FAT_UNIV" | fat_parse_hex | while read -r a b; do norm_cpu_hex "$a" "$b"; done)" && echo YES || echo NO)"

  # ── A2) ★核心回归：arm64 排在**前面**的 fat。
  #        "取第一个 Mach header" 的老逻辑在这里会读到 arm64 → 假失败。
  #        字节级枚举不受顺序影响，必须两片都列出且判定为含 x86_64。
  local FAT_ARMFIRST='cafebabe000000020100000c0000000000004000021600000000000e010000070000000302160000021401800000000e'
  _expect "A2 arm64 在前的 fat：两片都枚举到" "arm64 x86_64" "$(_slices "$FAT_ARMFIRST")"
  _expect "A2 顺序颠倒仍判定含 x86_64"        "YES" \
          "$(has_x86 "$(printf '%s\n' "$FAT_ARMFIRST" | fat_parse_hex | while read -r a b; do norm_cpu_hex "$a" "$b"; done)" && echo YES || echo NO)"

  # ── A3) ★核心回归：arm64-only fat（Postgres.app 若是 arm64-only 就是这形态）
  local FAT_ARMONLY='cafebabe000000010100000c000000020000400000015af00000000e'
  _expect "A3 arm64-only fat 枚举"      "arm64e" "$(_slices "$FAT_ARMONLY")"
  _expect "A3 不含 x86_64 ⇒ 必须失败"   "NO" \
          "$(has_x86 "$(printf '%s\n' "$FAT_ARMONLY" | fat_parse_hex | while read -r a b; do norm_cpu_hex "$a" "$b"; done)" && echo YES || echo NO)"

  # ── A4) FAT_MAGIC_64（cafebabf，fat_arch 为 32 字节含 64 位 offset/size）
  local FAT64='cafebabf000000020100000700000003000000000000400000000000021401800000000e000000000100000c00000000000000000216000000000000015800000000000e00000000'
  _expect "A4 FAT_MAGIC_64 枚举"        "x86_64 arm64" "$(_slices "$FAT64")"

  # ── A5) thin Mach-O（x86_64，magic 字节 cffaedfe）⇒ 枚举为空 ⇒ 判定 thin
  local THIN_HEX='cffaedfe0700000103000000020000001800000098070000850020000000000019000000'
  _expect "A5 thin x86_64 ⇒ 无 fat slice" "" "$(_slices "$THIN_HEX")"
  # ── A6) thin arm64（cputype 0100000c）同样应判为 thin（架构靠 -h 解析，不靠 fat 头）
  local THIN_ARM_HEX='cffaedfe0c000001000000000200000013000000c00600008500200000000000'
  _expect "A6 thin arm64 ⇒ 无 fat slice" "" "$(_slices "$THIN_ARM_HEX")"

  # ── A7) ★抗误判：Java .class 也以 cafebabe 开头。bytes[4..7] 是 minor/major
  #        version：Java 8 = 0x00000034 = 52，**落在 nfat_arch 的"合理"区间里**，
  #        光靠 n 的范围根本挡不住（第一版自测就是在这里翻车的）。
  #        靠"第一片 cputype 必须在白名单里"才挡得住。
  local JAVA='cafebabe0000003400a10a002d005c09'
  _expect "A7 Java .class(cafebabe/n=52) 不误判为 fat" "" "$(_slices "$JAVA")"
  local JAVA17='cafebabe000000610021070002070004'
  _expect "A7 Java 17 .class(n=97>32) 不误判"          "" "$(_slices "$JAVA17")"
  # ── A8) 抗截断：声称 3 片但字节只够 1 片 ⇒ 有几片报几片，不崩不乱码
  local TRUNC='cafebabe00000003010000070000000300004000021401800000000e'
  _expect "A8 fat 头被截断 ⇒ 只报能解析出的片" "x86_64" "$(_slices "$TRUNC")"
  # ── A9) x86_64h（cpusubtype=8）与 arm64e（cpusubtype=2）子型识别
  _expect "A9 x86_64h 子型" "x86_64h" "$(norm_cpu_hex 01000007 00000008)"
  _expect "A9 arm64e 子型"  "arm64e"  "$(norm_cpu_hex 0100000c 00000002)"
  _expect "A9 arm64e 带 caps 位(80000002) 仍识别" "arm64e" "$(norm_cpu_hex 0100000c 80000002)"
  _expect "A9 x86_64 ALL"   "x86_64"  "$(norm_cpu_hex 01000007 00000003)"
  # ── A10) has_x86 语义：x86_64h 也算命中；arm64/arm64e 不算
  _expect "A10 has_x86(x86_64h)" "YES" "$(has_x86 'x86_64h' && echo YES || echo NO)"
  _expect "A10 has_x86(arm64e)"  "NO"  "$(has_x86 'arm64e'  && echo YES || echo NO)"
  _expect "A10 has_x86 多行含 x86_64" "YES" "$(has_x86 "$(printf 'arm64\nx86_64\n')" && echo YES || echo NO)"

  echo
  echo "===== B. arch_desc 形态串 ====="
  _expect "B1 universal 通过"   "universal(x86_64+arm64)" "$(arch_desc "$(printf 'x86_64\narm64')" "" 1)"
  _expect "B2 thin x86_64 通过" "x86_64(thin)"            "$(arch_desc "" "x86_64" 1)"
  _expect "B3 thin arm64 失败"  "arm64(thin)❌"           "$(arch_desc "" "arm64" 0)"
  _expect "B4 arm64-only fat 失败" "universal(arm64e)❌"  "$(arch_desc "arm64e" "" 0)"

  echo
  echo "===== C. mv_arch_section：多架构 dump 切片（★最易翻车）====="

  # ── C1) llvm 风格：**arm64 排在前面**的 -h 输出。
  #        老 parser 取第一个 Mach header 会读成 arm64 → 假失败。
  #        切段后必须拿到 X86_64 那片。
  local MULTI_H
  MULTI_H='payload/bin/ollama (architecture arm64):
Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64    ARM64        ALL  0x00     EXECUTE    30       4520   NOUNDEFS DYLDLINK TWOLEVEL PIE
Load command 9
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 14.0
      sdk 26.0
payload/bin/ollama (architecture x86_64):
Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE    28       4200   NOUNDEFS DYLDLINK TWOLEVEL PIE
Load command 9
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 14.0
      sdk 26.0'
  local SEC_X SEC_A
  SEC_X="$(printf '%s\n' "$MULTI_H" | mv_arch_section x86_64)"
  SEC_A="$(printf '%s\n' "$MULTI_H" | mv_arch_section arm64)"
  _expect "C1 arm64 在前：切出的 x86_64 段 cpu"  "X86_64" "$(_get "$SEC_X" cpu)"
  _expect "C1 切出的 x86_64 段 ncmds 归属正确"   "1"      "$(printf '%s\n' "$SEC_X" | grep -c '4200')"
  _expect "C1 x86_64 段不含 arm64 的行"          "0"      "$(printf '%s\n' "$SEC_X" | grep -c 'ARM64')"
  _expect "C1 切出的 arm64 段 cpu"               "ARM64"  "$(_get "$SEC_A" cpu)"
  # ★ 回归说明：不切段时 mv_parse 取的是**第一片**（first-wins 守卫保证确定性）。
  #   这里第一片是 arm64 ⇒ 若不切段就会把 universal 误判成 arm64 而假失败。
  #   这条断言的意义是把"必须切段"这件事钉死：一旦有人把 mv_arch_section 摘掉，
  #   它会连同 C1 前几条一起变红。
  _expect "C1 不切段 ⇒ 读到第一片 arm64（故必须切段）" "ARM64" "$(_get "$MULTI_H" cpu)"
  _expect "C1 不切段 ⇒ minos 也来自第一片"        "14.0"   "$(_get "$MULTI_H" minos)"

  # ── C2) cctools 风格（宿主 arch 不在 fat 里时会打印全部 slice）+ -l 段落。
  #        dump_headers 会把 -h 与 -l 两次输出拼接，因此同一 arch 会出现两段，
  #        切段必须把两段都收进来（这样 minos 才取得到）。
  local MULTI_HL
  MULTI_HL='pg (architecture x86_64):
Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE    22       3000   NOUNDEFS
pg (architecture arm64):
Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64    ARM64        ALL  0x00     EXECUTE    22       3000   NOUNDEFS
pg (architecture x86_64):
Load command 8
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 10.15
      sdk 14.0
pg (architecture arm64):
Load command 8
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 11.0
      sdk 14.0'
  local S2X S2A
  S2X="$(printf '%s\n' "$MULTI_HL" | mv_arch_section x86_64)"
  S2A="$(printf '%s\n' "$MULTI_HL" | mv_arch_section arm64)"
  _expect "C2 -h/-l 分两批：x86_64 段 cpu"    "X86_64" "$(_get "$S2X" cpu)"
  _expect "C2 x86_64 段 minos（跨段收集）"    "10.15"  "$(_get "$S2X" minos)"
  _expect "C2 arm64 段 minos 不串味"          "11.0"   "$(_get "$S2A" minos)"

  # ── C3) 单架构 dump（-arch x86_64 或 thin 文件）没有分隔行 ⇒ 原样透传
  local SINGLE
  SINGLE='payload/bin/psql:
Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE    22       3000   NOUNDEFS
Load command 8
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos 10.15
      sdk 14.0'
  _expect "C3 无分隔行 ⇒ 透传，cpu"   "X86_64" "$(_get "$(printf '%s\n' "$SINGLE" | mv_arch_section x86_64)" cpu)"
  _expect "C3 无分隔行 ⇒ 透传，minos" "10.15"  "$(_get "$(printf '%s\n' "$SINGLE" | mv_arch_section x86_64)" minos)"

  # ── C4) 请求的 arch 在 dump 里不存在 ⇒ 切出空段（不得回退成别的 arch）
  _expect "C4 请求不存在的 arch ⇒ 空段" "0" \
          "$(printf '%s\n' "$MULTI_H" | mv_arch_section i386 | grep -c 'Mach header')"

  # ── C5) -L 多架构输出按 slice 切依赖
  local MULTI_L
  MULTI_L='payload/bin/ollama (architecture x86_64):
	/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1345.100.2)
	@rpath/libggml-base.dylib (compatibility version 0.0.0, current version 0.0.0)
payload/bin/ollama (architecture arm64):
	/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1345.100.2)
	/System/Library/Frameworks/Metal.framework/Versions/A/Metal (compatibility version 1.0.0, current version 1.0.0)
	@rpath/libmlx.dylib (compatibility version 0.0.0, current version 0.0.0)'
  _expect "C5 -L 切 x86_64：依赖数" "2" \
          "$(printf '%s\n' "$MULTI_L" | mv_arch_section x86_64 | mv_dylibs | grep -c .)"
  _expect "C5 -L 切 arm64：依赖数"  "3" \
          "$(printf '%s\n' "$MULTI_L" | mv_arch_section arm64  | mv_dylibs | grep -c .)"
  _expect "C5 x86_64 片不含 Metal"  "0" \
          "$(printf '%s\n' "$MULTI_L" | mv_arch_section x86_64 | grep -c 'Metal')"

  echo
  echo "===== D. 单架构 header 解析（沿用 run #17 的回归用例）====="

  # --- D1) cctools otool -h -v + -l，正确的 10.15 产物（期望形态）------------
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
  _expect "D1 cctools/good: cpu"      "X86_64"           "$(_get "$CCTOOLS_GOOD" cpu)"
  _expect "D1 cctools/good: minos"    "10.15"            "$(_get "$CCTOOLS_GOOD" minos)"
  _expect "D1 cctools/good: sdk"      "26.1"             "$(_get "$CCTOOLS_GOOD" sdk)"
  _expect "D1 cctools/good: lc"       "LC_BUILD_VERSION" "$(_get "$CCTOOLS_GOOD" lc)"
  _expect "D1 cctools/good: magic"    "MH_MAGIC_64"      "$(_get "$CCTOOLS_GOOD" magic)"
  # LC_SOURCE_VERSION 的 `version 0.0` 与 LC_BUILD_VERSION 的 `version 1053.12`
  # 都不得污染 minos —— 上面 minos=10.15 已经覆盖该回归点。

  # --- D2) llvm-objdump 数值形态 --------------------------------------------
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
  _expect "D2 llvm/good: cpu(numeric)"  "16777223"   "$(_get "$LLVM_GOOD" cpu)"
  _expect "D2 llvm/good: minos"         "10.15"      "$(_get "$LLVM_GOOD" minos)"
  _expect "D2 llvm/good: magic"         "0xfeedfacf" "$(_get "$LLVM_GOOD" magic)"
  _expect "D2 llvm/good: norm_cpu"      "x86_64"     "$(norm_cpu "$(_get "$LLVM_GOOD" cpu)")"

  # --- D3) ★核心回归：ld64 把 minos 悄悄提到 26.1（必须被抓到）--------------
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
  _expect "D3 BAD minos 26.1 被正确提取"  "26.1"   "$(_get "$BAD_MINOS" minos)"
  local m; m="$(norm_ver "$(_get "$BAD_MINOS" minos)")"
  _expect "D3 BAD minos != 10.15 (会硬失败)" "MISMATCH" \
          "$([ "$m" = "10.15" ] && echo MATCH || echo MISMATCH)"

  # --- D4) ★核心回归：arm64 产物必须被抓到 ----------------------------------
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
  _expect "D4 arm64 被识别"  "arm64"  "$(norm_cpu "$(_get "$ARM" cpu)")"

  # --- D5) 旧格式 LC_VERSION_MIN_MACOSX（字段名是 version 不是 minos）-------
  local LEGACY
  LEGACY='Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE    20       2400   NOUNDEFS
Load command 7
      cmd LC_VERSION_MIN_MACOSX
  cmdsize 16
  version 10.13
      sdk 26.1'
  _expect "D5 legacy: minos 取自 version"  "10.13" "$(_get "$LEGACY" minos)"
  _expect "D5 legacy: lc 标注来源"  "LC_VERSION_MIN_MACOSX" "$(_get "$LEGACY" lc)"

  # --- D6) 完全没有版本 load command（无法验证 ⇒ 必须报 NONE 而不是静默通过）-
  local NOVER
  NOVER='Mach header
      magic  cputype cpusubtype  caps    filetype ncmds sizeofcmds      flags
MH_MAGIC_64   X86_64        ALL  0x00     EXECUTE     5        400   NOUNDEFS
Load command 0
      cmd LC_SEGMENT_64
  cmdsize 72'
  _expect "D6 无版本 LC: minos=?"  "?"    "$(_get "$NOVER" minos)"
  _expect "D6 无版本 LC: lc=NONE"  "NONE" "$(_get "$NOVER" lc)"

  # --- D7) norm_cpu 认 cctools fat 头里的 CPU_TYPE_* 符号名 -----------------
  _expect "D7 norm_cpu CPU_TYPE_X86_64" "x86_64" "$(norm_cpu CPU_TYPE_X86_64)"
  _expect "D7 norm_cpu CPU_TYPE_ARM64"  "arm64"  "$(norm_cpu CPU_TYPE_ARM64)"

  echo
  echo "===== E. 版本号归一化与数值比较 ====="
  _expect "E1 norm_ver 10.15.0 -> 10.15" "10.15" "$(norm_ver '10.15.0')"
  _expect "E1 norm_ver 10.15   -> 10.15" "10.15" "$(norm_ver '10.15')"
  _expect "E1 norm_ver 26.1    -> 26.1"  "26.1"  "$(norm_ver '26.1')"
  # ★ 必须**分段数值**比较：10.15(Catalina) 比 10.9(Mavericks) 新，
  #   但字符串比较会得出 "10.15" < "10.9"（第 4 个字符 '1' < '9'）—— 正好反了。
  _expect "E2 ver_gt 10.15 10.9  = true"  "YES" "$(ver_gt 10.15 10.9  && echo YES || echo NO)"
  _expect "E2 ver_gt 10.9  10.15 = false" "NO"  "$(ver_gt 10.9  10.15 && echo YES || echo NO)"
  # 同一组输入下字符串比较的结论是错的 —— 钉住"不能退回字符串比较"
  _expect "E2 反例：字符串比较会判反"     "WRONG" \
          "$([ "10.15" \> "10.9" ] && echo RIGHT || echo WRONG)"
  _expect "E2 ver_gt 14.0  10.15 = true"  "YES" "$(ver_gt 14.0  10.15 && echo YES || echo NO)"
  _expect "E2 ver_gt 10.15 10.15 = false" "NO"  "$(ver_gt 10.15 10.15 && echo YES || echo NO)"
  _expect "E2 ver_gt 11    10.15 = true"  "YES" "$(ver_gt 11    10.15 && echo YES || echo NO)"
  _expect "E2 ver_gt 10.15 11    = false" "NO"  "$(ver_gt 10.15 11    && echo YES || echo NO)"

  echo
  echo "===== F. B3 路径泄漏检测 ====="
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
  _expect "F1 leak: 依赖条数"      "5" "$n_dylib"
  _expect "F1 leak: 命中黑名单数"  "2" "$leaks"
  local clean
  clean="$(printf '%s\n' "$LEAKY" | mv_dylibs | grep -Ev "$LEAK_RE" | grep -Evc "$SANE_RE" || true)"
  _expect "F1 leak: 白名单外且非泄漏 = 0" "0" "$clean"

  echo
  echo "self-test: ${pass} passed, ${fail} failed"
  [ "$fail" -eq 0 ]
}

# =============================================================================
# 第四部分：工具探测
# 用「能否在真实目标文件上产出 'Mach header'」作为判据，比 `--version` 之类的
# 探测靠谱（cctools otool 根本不支持 --version）。
# =============================================================================
OTOOL=""; OBJDUMP=""; MODE=""; ARCH_FLAG_OK=0

dump_headers() {
  if [ "$MODE" = "otool" ]; then
    "$OTOOL" -h -v "$1" 2>/dev/null
    "$OTOOL" -l "$1"    2>/dev/null
  else
    "$OBJDUMP" --macho --private-headers "$1" 2>/dev/null
  fi
}

# dump_headers_arch — 只取指定 slice 的 header。$1=arch $2=path
dump_headers_arch() {
  if [ "$MODE" = "otool" ]; then
    "$OTOOL" -arch "$1" -h -v "$2" 2>/dev/null
    "$OTOOL" -arch "$1" -l "$2"    2>/dev/null
  else
    "$OBJDUMP" --macho --arch="$1" --private-headers "$2" 2>/dev/null
  fi
}

# dump_headers_all — 强制打印所有 slice（带 (architecture X): 分隔行）
dump_headers_all() {
  if [ "$MODE" = "otool" ]; then
    "$OTOOL" -arch all -h -v "$1" 2>/dev/null
    "$OTOOL" -arch all -l "$1"    2>/dev/null
  else
    "$OBJDUMP" --macho --arch=all --private-headers "$1" 2>/dev/null
  fi
}

dump_dylibs() {
  if [ "$MODE" = "otool" ]; then
    "$OTOOL" -L "$1" 2>/dev/null
  else
    "$OBJDUMP" --macho --dylibs-used "$1" 2>/dev/null
  fi
}

dump_dylibs_arch() {  # $1=arch $2=path
  if [ "$MODE" = "otool" ]; then
    "$OTOOL" -arch "$1" -L "$2" 2>/dev/null
  else
    "$OBJDUMP" --macho --arch="$1" --dylibs-used "$2" 2>/dev/null
  fi
}

dump_fat() {  # 仅用于人类可读日志；判定逻辑一律走 fat_slices（字节解析）
  if [ "$MODE" = "otool" ]; then
    "$OTOOL" -f -v "$1" 2>/dev/null
  else
    "$OBJDUMP" --macho --universal-headers "$1" 2>/dev/null
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

  # ⚠️ `set -u` 下对**空数组**做 "${arr[@]}" 展开，在 bash 3.2（macOS 自带）会报
  #    "unbound variable" 并直接杀掉脚本 —— 于是「找不到解析工具」这条本该清晰
  #    的错误，会变成一句莫名其妙的 line NNN 报错。runner 的 bash 5.x 不触发，
  #    所以此前一直没暴露。这里统一先判长度再展开，让失败路径保持可读。
  if [ "${#cands[@]}" -gt 0 ]; then
    for c in "${cands[@]}"; do
      command -v "$c" >/dev/null 2>&1 || continue
      OTOOL="$c"; MODE="otool"
      if dump_headers "$probe" | grep -q "^Mach header"; then
        echo "macho tool: ${OTOOL} (mode=otool)"; return 0
      fi
    done
  fi

  local ocands=()
  while IFS= read -r c; do [ -n "$c" ] && ocands+=("$c"); done < <(
    { command -v llvm-objdump 2>/dev/null
      ls /usr/bin/llvm-objdump-* 2>/dev/null
      ls /usr/lib/llvm-*/bin/llvm-objdump 2>/dev/null; } | sort -Vu
  )
  if [ "${#ocands[@]}" -gt 0 ]; then
    for c in "${ocands[@]}"; do
      OBJDUMP="$c"; MODE="objdump"
      if dump_headers "$probe" | grep -q "^Mach header"; then
        echo "macho tool: ${OBJDUMP} (mode=objdump)"; return 0
      fi
    done
  fi

  OTOOL=""; OBJDUMP=""; MODE=""
  return 1
}

# =============================================================================
# 第五部分：取「指定 slice」的 header / dylib 文本（三级降级，绝不读错 slice）
# =============================================================================
# $1=slice 名（空表示 thin，直接默认输出） $2=path
headers_for_slice() {
  local sel="$1" path="$2" out=""
  if [ -z "$sel" ]; then
    dump_headers "$path"; return
  fi
  # ① 首选 -arch <slice>：输出干净的单架构文本
  out="$(dump_headers_arch "$sel" "$path")"
  if printf '%s\n' "$out" | grep -q "^Mach header"; then printf '%s\n' "$out"; return; fi
  # ② 工具不支持 -arch ⇒ -arch all 后按分隔行切段
  out="$(dump_headers_all "$path" | mv_arch_section "$sel")"
  if printf '%s\n' "$out" | grep -q "^Mach header"; then printf '%s\n' "$out"; return; fi
  # ③ 连 -arch all 都不认 ⇒ 默认输出后切段（cctools 无宿主 arch 时就是这形态）
  out="$(dump_headers "$path" | mv_arch_section "$sel")"
  if printf '%s\n' "$out" | grep -q "^Mach header"; then printf '%s\n' "$out"; return; fi
  # ④ 全部失败：返回空，由调用方标记为 minos 不可判定（绝不拿别的 slice 冒充）
  printf ''
}

dylibs_for_slice() {
  local sel="$1" path="$2" out=""
  if [ -z "$sel" ]; then
    dump_dylibs "$path"; return
  fi
  out="$(dump_dylibs_arch "$sel" "$path")"
  if printf '%s\n' "$out" | grep -q "compatibility version"; then printf '%s\n' "$out"; return; fi
  out="$(dump_dylibs "$path" | mv_arch_section "$sel")"
  printf '%s\n' "$out"
}

# =============================================================================
# 第六部分：主流程
# =============================================================================
main() {
  local sidecar=0
  if [ "${1:-}" = "--sidecar" ]; then sidecar=1; shift; fi

  local expect_arch="$1"; shift
  local expect_minos="$1"; shift
  local n_fail=0
  local tag min_bytes
  if [ "$sidecar" = "1" ]; then tag="sidecar_check"; min_bytes="$SIDECAR_MIN_BYTES"
  else                          tag="macho_check";   min_bytes="$MACHO_MIN_BYTES"; fi

  if [ "$#" -eq 0 ]; then
    echo "::error::verify-macho: 未传入任何待检查的二进制"
    return 1
  fi

  # ---- B4 先行：文件必须存在（否则连工具探测都无从谈起）--------------------
  local spec label path first=""
  for spec in "$@"; do
    label="${spec%%=*}"; path="${spec#*=}"
    if [ ! -f "$path" ]; then
      echo "::error::${tag} ${label}: FILE MISSING — 期望路径 ${path} 不存在（打包装配未产出该二进制）"
      echo "::notice::${tag} ${label}: arch=MISSING minos=? size=0"
      n_fail=$((n_fail + 1))
    elif [ -z "$first" ]; then
      first="$path"
    fi
  done
  if [ -z "$first" ]; then
    echo "::error::${tag}: 所有待检查二进制都不存在，无法继续"
    return 1
  fi

  if ! detect_tool "$first"; then
    echo "::error::${tag}: 找不到可用的 Mach-O 解析工具（试过 osxcross otool / llvm-otool / otool / llvm-objdump）。无法验证 cputype 与 minos —— 这两项是产物能否在用户机启动的前提，因此**不允许静默放行**。"
    return 1
  fi

  for spec in "$@"; do
    label="${spec%%=*}"; path="${spec#*=}"
    [ -f "$path" ] || continue

    local size slices sel is_fat thin_arch b1_ok
    size="$(file_size "$path")"; size="${size:-0}"

    # ---- ★ 形态判定：字节级解析 fat header（不依赖工具默认行为）-----------
    slices="$(fat_slices "$path")"
    if [ -n "$slices" ]; then is_fat=1; else is_fat=0; fi

    # ---- 选出要深入检查的 slice ------------------------------------------
    #      fat ⇒ 挑 x86_64（用户机架构）；thin ⇒ 空串（走默认输出）
    sel=""
    if [ "$is_fat" = "1" ]; then
      sel="$(printf '%s\n' "$slices" | grep -E '^x86_64(h)?$' | head -1)"
      # fat 里没有 x86_64：仍取第一片做诊断，让注解里能看到它到底是什么
      [ -n "$sel" ] || sel="$(printf '%s\n' "$slices" | head -1)"
    fi

    local hdr magic cpu plat minos sdk lc arch
    hdr="$(headers_for_slice "$sel" "$path")"
    magic="$(printf '%s\n' "$hdr" | mv_parse | grep '^magic='    | cut -d= -f2-)"
    cpu="$(  printf '%s\n' "$hdr" | mv_parse | grep '^cpu='      | cut -d= -f2-)"
    plat="$( printf '%s\n' "$hdr" | mv_parse | grep '^platform=' | cut -d= -f2-)"
    minos="$(printf '%s\n' "$hdr" | mv_parse | grep '^minos='    | cut -d= -f2-)"
    sdk="$(  printf '%s\n' "$hdr" | mv_parse | grep '^sdk='      | cut -d= -f2-)"
    lc="$(   printf '%s\n' "$hdr" | mv_parse | grep '^lc='       | cut -d= -f2-)"
    arch="$(norm_cpu "$cpu")"
    thin_arch="$arch"

    # 诚实性守卫：若走到了 headers_for_slice 的第 ③ 级降级，而该工具对 fat 文件
    # 默认只打印某一片且不带 (architecture X): 分隔行，切段就成了原样透传 ——
    # 此时拿到的 minos 可能不属于我们想要的 slice。B1 的判定不受影响（它只依赖
    # od 解析出的 slice 列表），但 minos 必须标注为存疑，不能假装精确。
    if [ "$is_fat" = "1" ] && [ -n "$sel" ] && [ -n "$hdr" ]; then
      case "$sel" in
        x86_64*) [ "$arch" = "x86_64" ] || echo "::warning::${tag} ${label}: 目标 slice 是 ${sel}，但解析到的 Mach header 是 ${arch} —— 工具未能按 slice 定位，下面的 minos/sdk/dylibs 可能取自其它 slice（B1 架构判定不受影响，它基于字节级 fat 头解析）。" ;;
      esac
    fi

    # ---- B1 判定：thin 看 cputype，fat 看 slice 列表里有没有 x86_64 -------
    b1_ok=0
    if [ "$is_fat" = "1" ]; then
      has_x86 "$slices" && b1_ok=1
    else
      [ "$arch" = "$expect_arch" ] && b1_ok=1
    fi
    local form; form="$(arch_desc "$slices" "$thin_arch" "$b1_ok")"

    # ---- B3：依赖清单与泄漏 ----------------------------------------------
    local dl n_dylib leak_list odd_list
    dl="$(dylibs_for_slice "$sel" "$path")"
    n_dylib="$(printf '%s\n' "$dl" | mv_dylibs | grep -c . || true)"
    leak_list="$(printf '%s\n' "$dl" | mv_dylibs | grep -E "$LEAK_RE" | tr '\n' ',' | sed 's/,$//' || true)"
    odd_list="$( printf '%s\n' "$dl" | mv_dylibs | grep -Ev "$LEAK_RE" | grep -Ev "$SANE_RE" \
                 | tr '\n' ',' | sed 's/,$//' || true)"

    # ---- 无条件打印关键指标（即使全绿也打），下一轮可直接从注解读事实 -----
    if [ "$sidecar" = "1" ]; then
      echo "::notice::${tag} ${label}: arch=${form} minos=${minos} sdk=${sdk} lc=${lc} dylibs=${n_dylib} leak=${leak_list:-none} size=$(human "$size")"
    else
      echo "::notice::${tag} ${label}: cputype=${arch} form=${form} minos=${minos} sdk=${sdk} lc=${lc} platform=${plat} dylibs=${n_dylib} leak=${leak_list:-none} size=$(human "$size")"
    fi
    echo "--- ${label} (${path}) slice=${sel:-thin} ---"
    [ "$is_fat" = "1" ] && { dump_fat "$path" | head -30 || true; }
    printf '%s\n' "$dl" | head -40 || true

    # ---- B1 架构（两种模式都硬失败）--------------------------------------
    if [ -z "$hdr" ]; then
      echo "::error::${tag} ${label}: B1 FAIL — 无法取到 slice ${sel} 的 Mach header（工具三级降级全部失败）。不允许在读不到头的情况下静默放行。"
      n_fail=$((n_fail + 1))
    elif ! magic_ok "$magic"; then
      echo "::error::${tag} ${label}: B1 FAIL — Mach-O magic 非法：期望 MH_MAGIC_64/0xfeedfacf，实际 ${magic}（${path} 可能不是 64 位 Mach-O，或根本不是 Mach-O）"
      n_fail=$((n_fail + 1))
    fi
    if [ "$b1_ok" != "1" ]; then
      if [ "$is_fat" = "1" ]; then
        echo "::error::${tag} ${label}: B1 FAIL — universal 二进制里**没有 x86_64 slice**（实际 slice = $(printf '%s' "$slices" | tr '\n' ',' | sed 's/,$//')）。用户机是 Intel x86_64，该二进制在用户机上无法执行。这是上游分发件的架构问题，**不要自动改下载源**，请把此事实带回评审后再定对策。"
      else
        echo "::error::${tag} ${label}: B1 FAIL — cputype 期望 ${expect_arch} 实际 ${arch}（raw=${cpu}，thin 单架构）。用户机是 Intel x86_64，架构不符则该组件在用户机上无法执行。"
      fi
      n_fail=$((n_fail + 1))
    fi

    # ---- B2 minos ---------------------------------------------------------
    if [ "$sidecar" = "1" ]; then
      # sidecar：只打印、不 gate。上游 deployment target 不归我们管，
      #          拿 10.15 硬 gate 会误杀。但高于目标时给一条 warning。
      # ⚠️ 比较基准用 BUNDLE_MIN_MACOS（默认 11.0）而非 expect_minos(=10.15)：
      #   打过 minos 补丁后，ollama 等 sidecar 的真实下限=11.0，若仍拿 10.15
      #   去比，会把它「高于本包宣称值」误报出来（run #22 修复点）。
      if [ "$lc" = "NONE" ] || [ "$minos" = "?" ]; then
        echo "::warning::${tag} ${label}: B2 INFO — 未找到 LC_BUILD_VERSION / LC_VERSION_MIN_MACOSX，无法判定 deployment target（仅记录，不阻断）。"
      elif ver_gt "$(norm_ver "$minos")" "$(norm_ver "$BUNDLE_MIN_MACOS")"; then
        echo "::warning::${tag} ${label}: B2 INFO — 上游 minos=${minos} **高于**本包宣称的 ${BUNDLE_MIN_MACOS}。该 sidecar 在低于 macOS ${minos} 的机器上会被 dyld 拒绝启动，意味着「本包实际可用的最低系统」被这个上游件抬到了 ${minos}。不 gate（上游 deployment target 非我方可控），但 Info.plist 的 LSMinimumSystemVersion 与对外说明需要据此校准。"
      fi
    else
      # strict：硬失败。链接成功 ≠ deployment target 生效。
      if [ "$lc" = "NONE" ] || [ "$minos" = "?" ]; then
        echo "::error::${tag} ${label}: B2 FAIL — 二进制里既没有 LC_BUILD_VERSION 也没有 LC_VERSION_MIN_MACOSX，无法确认 deployment target。不允许静默放行。"
        n_fail=$((n_fail + 1))
      elif [ "$(norm_ver "$minos")" != "$(norm_ver "$expect_minos")" ]; then
        echo "::error::${tag} ${label}: B2 FAIL — minos 期望 ${expect_minos} 实际 ${minos}（来源 ${lc}，sdk=${sdk}）。deployment target 未按 MACOSX_DEPLOYMENT_TARGET 生效：若实际值高于期望值，该二进制会被用户机的 dyld 直接拒绝启动（\"requires macOS ${minos} or later\"）。这正是「链接成功但产物不可用」的典型形态，必须在发包前拦下。"
        n_fail=$((n_fail + 1))
      fi
    fi

    # ---- B3 路径泄漏 ------------------------------------------------------
    if [ -n "$leak_list" ]; then
      if [ "$sidecar" = "1" ]; then
        echo "::warning::${tag} ${label}: B3 INFO — LC_LOAD_DYLIB 含疑似本机绝对路径：${leak_list}。上游二进制的 install name 非我方可控（常见于 /opt/homebrew、/opt/local）；若用户机缺这些库会报 'Library not loaded'。**仅记录不阻断**，请评审后决定是否换源或做 install_name_tool 重写。"
      else
        echo "::error::${tag} ${label}: B3 FAIL — LC_LOAD_DYLIB 含构建机本地路径：${leak_list}。这些路径在用户 Mac 上不存在，dyld 会报 'Library not loaded' 而启动失败。"
        n_fail=$((n_fail + 1))
      fi
    fi
    if [ -n "$odd_list" ]; then
      echo "::warning::${tag} ${label}: B3 WARN — 依赖路径不在已知安全前缀内（非致命，仅供核对）：${odd_list}"
    fi

    # ---- B4 非空 / 未截断（硬失败）---------------------------------------
    if [ "$size" -lt "$min_bytes" ]; then
      echo "::error::${tag} ${label}: B4 FAIL — 体积 ${size} B（$(human "$size")）低于下限 ${min_bytes} B（$(human "$min_bytes")）。疑似空文件 / 截断 / 误把占位脚本打进包。"
      n_fail=$((n_fail + 1))
    fi
  done

  if [ "$n_fail" -ne 0 ]; then
    echo "::error::${tag}: 共 ${n_fail} 条断言失败 —— 产物不可交付，已阻止上传 artifact。"
    return 1
  fi
  if [ "$sidecar" = "1" ]; then
    echo "${tag}: 全部 sidecar 断言通过（每个都含 x86_64 slice；minos 仅记录不 gate）"
  else
    echo "${tag}: 全部断言通过（arch=${expect_arch}, minos=${expect_minos}）"
  fi
  return 0
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi
main "$@"
exit $?
