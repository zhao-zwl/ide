#!/usr/bin/env bash
# =============================================================================
# 把 cargo 日志里的编译/链接错误行转成 GitHub ::error:: 注解。
#
# 为什么需要它：job 日志与 artifact 都要鉴权才能读，只有 annotations 是公开可读的。
# 之前的实现把 tail -60 压成**一条**注解，导致 `--message-format` 的 file:line
# 被挤掉、错误被截断，只能靠 CI 往返「挤牙膏」。
#
# 用法：
#   annotate-cargo-errors.sh <log-file> <label> [max-batches]
#
# 行为：
#   * 【第一步永远是剥 ANSI 色码】见下面「run #14 血的教训」
#   * 优先抓 `--message-format=short` 的单行诊断 `path/to/f.rs:12:34: error[E0425]: ...`
#   * 同时抓链接期诊断（ld: / framework not found / Undefined symbols / clang: error:）
#   * 折叠**完全相同**的重复行（LTO on/off 两轮会产出同一批错误），并同时汇报
#     原始行数与去重后行数 —— 不丢信息，只去冗余
#   * 每条注解塞 PER_BATCH(默认 5) 条错误，最多 max-batches(默认 12) 条注解。
#     GitHub 单个 step 在 UI 上每个级别只展示前 10 条注解，因此**分批**比
#     「每错误一条注解」更能保住 file:line 不被截断。
#   * 额外吐一条自检探针 `rustc_diag_lines=N`：解析到多少条形如 error[EXXXX]
#     的 rustc 诊断。若 cargo 说 "due to N previous errors" 而这里是 0，
#     就能一眼判定是解析层（剥色/正则）挂了，而不是错误本身消失了。
#
# -----------------------------------------------------------------------------
# 【run #14 血的教训 —— grep 之前必须剥 ANSI 色码，任何人不要改回去】
# -----------------------------------------------------------------------------
#   .github/workflows/build-dmg.yml 顶层 env 设了 `CARGO_TERM_COLOR: always`，
#   于是 `--message-format=short` 的诊断行实际长这样（ESC = 0x1B）：
#
#     gui/src-tauri/src/a.rs:12:34: ESC[0mESC[1mESC[38;5;9merror[E0308]ESC[0m: mismatched types
#
#   下面 ERR_RE 的两个主分支都会**静默失配**：
#     * `(^|[[:space:]])error(\[...\])?:`
#         → `error` 前面紧挨着 `ESC[38;5;9m`，既不是行首也不是空白 ⇒ 不匹配
#     * `^[^[:space:]]+:[0-9]+:[0-9]+:[[:space:]]*error`
#         → `[[:space:]]*` 之后遇到的是 ESC 而不是 `error` ⇒ 不匹配
#
#   结果只有字面量子串分支（`could not compile`）幸存，4 条真实错误的
#   file:line:col 全部丢失 —— 注解里只剩一句「due to 4 previous errors」，
#   白烧一整轮 CI（run #14，commit 9be10b1）。
#
#   实测同一份样本：带色码 grep 命中 1 行；剥色后命中 3 行且 file:line:col 完整。
#   ⇒ 所有 grep / tail 之前，日志必须先过 strip_ansi_into()。
#   （workflow 侧另有一层保险：step 级 `CARGO_TERM_COLOR: never`。两层任一
#     生效都能拿到位置信息，但**不要**因此删掉本文件的剥色 —— 谁都可能
#     在别处忘了设 env，而这里是最后一道防线。）
#
# 本脚本自身绝不因为诊断失败而让调用方挂掉：始终以 0 退出。
# =============================================================================
set -uo pipefail

LOG="${1:-}"
LABEL="${2:-cargo}"
MAX_BATCHES="${3:-12}"
# 剥色之后单行会明显变长（`error[E0277]: the trait bound ... is not satisfied`
# 动辄 150+ 字符）。8×150 已逼近 MAX_ANNOTATION_CHARS，尾部会被 cut 截掉 ——
# 好不容易救回来的 file:line 别又被截没，所以每批只放 5 条。
PER_BATCH="${PER_BATCH:-5}"
MAX_ANNOTATION_CHARS="${MAX_ANNOTATION_CHARS:-3500}"

if [ -z "$LOG" ] || [ ! -f "$LOG" ]; then
  echo "::error::${LABEL} 无法读取日志文件 '${LOG}'（注解生成器拿不到输入）"
  echo "::error::${LABEL} rustc_diag_lines=0 (no input log)"
  exit 0
fi

# -----------------------------------------------------------------------------
# strip_ansi_into <src> <dst> —— 去掉 CSI 序列（ESC[ ... 终结符）、OSC 序列
# （ESC] ... BEL）以及裸的双字符转义（ESC 后跟单个 @-Z / \ / ^ / _）。
# 主实现用 perl（ubuntu-latest 必装）；perl 缺失时退化到 sed。
# 返回非 0 表示剥色失败，调用方需自行兜底。
# -----------------------------------------------------------------------------
strip_ansi_into() {
  local src="$1" dst="$2"
  if command -v perl >/dev/null 2>&1; then
    perl -pe 's/\e\[[0-9;:?]*[ -\/]*[\@-~]//g; s/\e\][^\a\e]*(?:\a|\e\\)?//g; s/\e[\@-Z\\-_]//g' \
      < "$src" > "$dst" 2>/dev/null && return 0
  fi
  local esc
  esc="$(printf '\033')"
  # 用 # 作分隔符：模式里 [ -/] 与 [@-~] 都不含字面量 '#'，无需转义。
  sed -E -e "s#${esc}\[[0-9;:?]*[ -/]*[@-~]##g" \
         -e "s#${esc}\][^${esc}]*##g" \
         < "$src" > "$dst" 2>/dev/null && return 0
  return 1
}

CLEAN="$(mktemp 2>/dev/null || echo "/tmp/annotate-clean.$$")"
ANSI_STRIPPED=yes
if ! strip_ansi_into "$LOG" "$CLEAN" || { [ -s "$LOG" ] && [ ! -s "$CLEAN" ]; }; then
  # 剥色器不可用 / 结果为空：宁可拿带色码的原文，也不能把日志整个丢掉。
  cat "$LOG" > "$CLEAN" 2>/dev/null || true
  ANSI_STRIPPED=no
  echo "::warning::${LABEL} ANSI 剥离失败，回退原始日志（file:line 可能仍被色码遮蔽）"
fi

# `--message-format=short` 的诊断行 + 链接期诊断 + panic。
# 末尾那条 `error\[E[0-9]{4}\]` 是**位置无关**的兜底分支：即便将来又有色码或
# 别的前缀混进来，只要 rustc 错误码还在，就还能捞到整行（含 file:line:col）。
ERR_RE='(^|[[:space:]])error(\[[A-Za-z0-9]+\])?:|^[^[:space:]]+:[0-9]+:[0-9]+:[[:space:]]*error|(^|[[:space:]])ld:[[:space:]]|framework not found|[Uu]ndefined symbols|symbol\(s\) not found|undefined reference to|clang: error:|panicked at|could not compile|error\[E[0-9]{4}\]'

RAW="$(grep -aE "$ERR_RE" "$CLEAN" 2>/dev/null | tr -d '\r')"

if [ -z "$RAW" ]; then
  # 没匹配到结构化错误：退化为 tail，至少给出**一些**可见上下文。
  # 注意这里读的是 $CLEAN 而不是 $LOG —— 否则退化路径会吐一屏色码乱码。
  fallback="$(tail -n 40 "$CLEAN" 2>/dev/null | tr -d '\r' | tr '\n\t' '  ' | tr -s ' ' | cut -c1-"$MAX_ANNOTATION_CHARS")"
  echo "::error::${LABEL} rustc_diag_lines=0 ansi_stripped=${ANSI_STRIPPED} (无任何结构化错误行)"
  echo "::error::${LABEL} 未匹配到结构化错误行；日志尾部：${fallback:-<empty>}"
  exit 0
fi

raw_count="$(printf '%s\n' "$RAW" | grep -c . || true)"
# 折叠完全相同的行（两轮 LTO 会重复同一批错误），保持首次出现顺序。
UNIQ="$(printf '%s\n' "$RAW" | awk 'NF && !seen[$0]++')"
uniq_count="$(printf '%s\n' "$UNIQ" | grep -c . || true)"

# 自检探针：真正带 rustc 错误码的诊断有几条。
# 若 cargo 说 "due to N previous errors" 而这里是 0 ⇒ 解析层还有问题
# （多半是 ANSI 没剥干净或正则失配），不必再靠猜。
diag_lines="$(printf '%s\n' "$UNIQ" | grep -cE 'error\[E[0-9]{4}\]' || true)"
with_pos="$(printf '%s\n' "$UNIQ" | grep -cE '^[^[:space:]]+:[0-9]+:[0-9]+:' || true)"
echo "::error::${LABEL} rustc_diag_lines=${diag_lines} with_file_line=${with_pos} distinct=${uniq_count} raw=${raw_count} ansi_stripped=${ANSI_STRIPPED}"

echo "::error::${LABEL} FAILED — ${uniq_count} distinct error line(s) (${raw_count} raw). 逐条 file:line 见下面的 [batch N] 注解。"

# 完整清单也打进 job log（artifact 可下载核对）。
echo "::group::${LABEL} distinct error lines (${uniq_count}, ansi_stripped=${ANSI_STRIPPED})"
printf '%s\n' "$UNIQ"
echo "::endgroup::"

batch=""
n=0
bn=0
while IFS= read -r line; do
  [ -n "$line" ] || continue
  line="$(printf '%s' "$line" | tr '\t' ' ' | tr -s ' ')"
  if [ -n "$batch" ]; then
    batch="${batch} ;; ${line}"
  else
    batch="${line}"
  fi
  n=$((n + 1))
  if [ "$((n % PER_BATCH))" -eq 0 ]; then
    bn=$((bn + 1))
    echo "::error::${LABEL}[batch ${bn}] $(printf '%s' "$batch" | cut -c1-"$MAX_ANNOTATION_CHARS")"
    batch=""
    if [ "$bn" -ge "$MAX_BATCHES" ]; then
      break
    fi
  fi
done <<EOF
$UNIQ
EOF

if [ -n "$batch" ] && [ "$bn" -lt "$MAX_BATCHES" ]; then
  bn=$((bn + 1))
  echo "::error::${LABEL}[batch ${bn}] $(printf '%s' "$batch" | cut -c1-"$MAX_ANNOTATION_CHARS")"
fi

# `n` 就是实际进过批次的行数（here-doc 循环跑在当前 shell，变量可见）。
emitted="$n"
if [ "$uniq_count" -gt "$emitted" ]; then
  echo "::error::${LABEL} 还有 $((uniq_count - emitted)) 条错误未进注解（超出 ${MAX_BATCHES} 批上限），完整清单见 job log / cargo-logs artifact。"
fi

exit 0
