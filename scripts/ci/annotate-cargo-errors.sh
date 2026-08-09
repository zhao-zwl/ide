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
#   * 优先抓 `--message-format=short` 的单行诊断 `path/to/f.rs:12:34: error[E0425]: ...`
#   * 同时抓链接期诊断（ld: / framework not found / Undefined symbols / clang: error:）
#   * 折叠**完全相同**的重复行（LTO on/off 两轮会产出同一批错误），并同时汇报
#     原始行数与去重后行数 —— 不丢信息，只去冗余
#   * 每条注解塞 PER_BATCH(默认 8) 条错误，最多 max-batches(默认 12) 条注解。
#     GitHub 单个 step 在 UI 上每个级别只展示前 10 条注解，因此**分批**比
#     「每错误一条注解」更能保住 file:line 不被截断。
#
# 本脚本自身绝不因为诊断失败而让调用方挂掉：始终以 0 退出。
# =============================================================================
set -uo pipefail

LOG="${1:-}"
LABEL="${2:-cargo}"
MAX_BATCHES="${3:-12}"
PER_BATCH="${PER_BATCH:-8}"
MAX_ANNOTATION_CHARS="${MAX_ANNOTATION_CHARS:-3500}"

if [ -z "$LOG" ] || [ ! -f "$LOG" ]; then
  echo "::error::${LABEL} 无法读取日志文件 '${LOG}'（注解生成器拿不到输入）"
  exit 0
fi

# `--message-format=short` 的诊断行 + 链接期诊断 + panic。
ERR_RE='(^|[[:space:]])error(\[[A-Za-z0-9]+\])?:|^[^[:space:]]+:[0-9]+:[0-9]+:[[:space:]]*error|(^|[[:space:]])ld:[[:space:]]|framework not found|[Uu]ndefined symbols|symbol\(s\) not found|undefined reference to|clang: error:|panicked at|could not compile'

RAW="$(grep -aE "$ERR_RE" "$LOG" 2>/dev/null | tr -d '\r')"

if [ -z "$RAW" ]; then
  # 没匹配到结构化错误：退化为 tail，至少给出**一些**可见上下文。
  fallback="$(tail -n 40 "$LOG" 2>/dev/null | tr -d '\r' | tr '\n\t' '  ' | tr -s ' ' | cut -c1-"$MAX_ANNOTATION_CHARS")"
  echo "::error::${LABEL} 未匹配到结构化错误行；日志尾部：${fallback:-<empty>}"
  exit 0
fi

raw_count="$(printf '%s\n' "$RAW" | grep -c . || true)"
# 折叠完全相同的行（两轮 LTO 会重复同一批错误），保持首次出现顺序。
UNIQ="$(printf '%s\n' "$RAW" | awk 'NF && !seen[$0]++')"
uniq_count="$(printf '%s\n' "$UNIQ" | grep -c . || true)"

echo "::error::${LABEL} FAILED — ${uniq_count} distinct error line(s) (${raw_count} raw). 逐条 file:line 见下面的 [batch N] 注解。"

# 完整清单也打进 job log（artifact 可下载核对）。
echo "::group::${LABEL} distinct error lines (${uniq_count})"
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

emitted=$((bn * PER_BATCH))
if [ "$uniq_count" -gt "$emitted" ]; then
  echo "::error::${LABEL} 还有 $((uniq_count - emitted)) 条错误未进注解（超出 ${MAX_BATCHES} 批上限），完整清单见 job log / cargo-logs artifact。"
fi

exit 0
