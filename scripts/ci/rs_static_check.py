#!/usr/bin/env python3
"""静态校验 Rust 源文件（无需 cargo/rustc）。

本机 Spotlight(mds) 死锁，禁止运行 cargo/rustc，故用纯 Python 做三类检查：

1. 分隔符配对：跳过字符串/字符/行注释/块注释后，校验 (){}[] 平衡。
2. 导入-使用交叉核对：`use a::{B, C};` 中的每个名字是否在文件正文中出现。
3. MutexGuard 跨 await 扫描：以缩进 + 花括号深度近似作用域，检出
   「某语句把 .lock() 的守卫绑定到变量，且同一作用域内其后存在 .await」。

用法：rs_static_check.py <file.rs> [more.rs ...]
退出码 0 表示全部通过。
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field
from typing import Iterable


# --------------------------------------------------------------------------
# 1) 词法预处理：剥离注释与字符串字面量，保留位置（用等长空格替换）
# --------------------------------------------------------------------------
def strip_noncode(src: str) -> str:
    """把注释/字符串/字符字面量替换为等长空白，保持字节偏移不变。"""
    out: list[str] = []
    i = 0
    n = len(src)
    while i < n:
        ch = src[i]
        nxt = src[i + 1] if i + 1 < n else ""
        # 行注释
        if ch == "/" and nxt == "/":
            j = src.find("\n", i)
            j = n if j == -1 else j
            out.append(" " * (j - i))
            i = j
            continue
        # 块注释（Rust 支持嵌套）
        if ch == "/" and nxt == "*":
            depth = 1
            j = i + 2
            while j < n and depth > 0:
                if src[j] == "/" and j + 1 < n and src[j + 1] == "*":
                    depth += 1
                    j += 2
                elif src[j] == "*" and j + 1 < n and src[j + 1] == "/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            seg = src[i:j]
            out.append("".join(c if c == "\n" else " " for c in seg))
            i = j
            continue
        # 原始字符串 r"..." / r#"..."#
        if ch == "r" and (nxt == '"' or nxt == "#"):
            k = i + 1
            hashes = 0
            while k < n and src[k] == "#":
                hashes += 1
                k += 1
            if k < n and src[k] == '"':
                term = '"' + "#" * hashes
                j = src.find(term, k + 1)
                j = n if j == -1 else j + len(term)
                seg = src[i:j]
                out.append("".join(c if c == "\n" else " " for c in seg))
                i = j
                continue
        # 普通字符串
        if ch == '"':
            j = i + 1
            while j < n:
                if src[j] == "\\":
                    j += 2
                    continue
                if src[j] == '"':
                    j += 1
                    break
                j += 1
            seg = src[i:j]
            out.append("".join(c if c == "\n" else " " for c in seg))
            i = j
            continue
        # 字符字面量（需与生命周期 'a 区分：字符字面量必有闭合单引号且长度短）
        if ch == "'":
            m = re.match(r"'(\\.|[^\\'])'", src[i:])
            if m:
                out.append(" " * len(m.group(0)))
                i += len(m.group(0))
                continue
        out.append(ch)
        i += 1
    return "".join(out)


# --------------------------------------------------------------------------
# 2) 分隔符配对
# --------------------------------------------------------------------------
PAIRS = {")": "(", "]": "[", "}": "{"}
OPENERS = set("([{")


def check_delimiters(path: str, code: str) -> list[str]:
    errs: list[str] = []
    stack: list[tuple[str, int]] = []
    line = 1
    for ch in code:
        if ch == "\n":
            line += 1
        elif ch in OPENERS:
            stack.append((ch, line))
        elif ch in PAIRS:
            if not stack:
                errs.append(f"{path}:{line}: 多余的闭合符 '{ch}'")
            elif stack[-1][0] != PAIRS[ch]:
                op, ol = stack.pop()
                errs.append(f"{path}:{line}: '{ch}' 与第 {ol} 行的 '{op}' 不匹配")
            else:
                stack.pop()
    for op, ol in stack:
        errs.append(f"{path}:{ol}: 未闭合的 '{op}'")
    return errs


# --------------------------------------------------------------------------
# 3) 导入-使用交叉核对
# --------------------------------------------------------------------------
USE_RE = re.compile(r"^\s*(?:pub\s+)?use\s+([^;]+);", re.MULTILINE)


def imported_names(code: str) -> list[tuple[str, int]]:
    """返回 [(被引入的最终标识符, 行号)]，忽略 glob 与 self。"""
    names: list[tuple[str, int]] = []
    for m in USE_RE.finditer(code):
        body = m.group(1)
        line = code[: m.start()].count("\n") + 1
        if "*" in body:
            continue
        # 展开 a::{B, C as D, e::F}
        leaves = re.findall(r"[A-Za-z_][A-Za-z0-9_]*(?:\s+as\s+[A-Za-z_][A-Za-z0-9_]*)?", body)
        # 只取「路径末段」：用 :: 与 , 切分
        parts = re.split(r"[{},]", body)
        for p in parts:
            p = p.strip()
            if not p or p in ("self", "crate", "super"):
                continue
            if " as " in p:
                p = p.split(" as ")[-1].strip()
            leaf = p.split("::")[-1].strip()
            if not leaf or leaf in ("self", "*"):
                continue
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", leaf):
                names.append((leaf, line))
        del leaves
    return names


# 这些是 trait：通过方法调用语法生效，正文中不会出现其名字。
# 纯文本扫描无法判断其是否被使用，故不报告（交给 rustc）。
METHOD_ONLY_TRAITS = {
    "Manager",
    "Emitter",
    "StoreExt",
    "StreamExt",
    "CommentStore",
    "LockStore",
    "ToolExecutor",
    "Validator",
    "HostBridge",
    "Llm",
}


def check_unused_imports(path: str, code: str) -> list[str]:
    warns: list[str] = []
    # 去掉所有 use 行后的正文，用于统计使用次数
    body = USE_RE.sub(lambda m: "\n" * m.group(0).count("\n"), code)
    for leaf, line in imported_names(code):
        if leaf in METHOD_ONLY_TRAITS:
            continue
        if not re.search(r"\b" + re.escape(leaf) + r"\b", body):
            warns.append(f"{path}:{line}: 导入但正文未使用 -> {leaf}")
    return warns


# --------------------------------------------------------------------------
# 4) MutexGuard 跨 await 扫描
# --------------------------------------------------------------------------
@dataclass
class GuardBinding:
    line: int
    text: str
    depth: int


@dataclass
class Finding:
    path: str
    guard: GuardBinding
    await_line: int
    await_text: str


BIND_GUARD_RE = re.compile(r"^\s*let\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)$")


def scan_guard_across_await(path: str, code: str) -> tuple[list[Finding], list[str]]:
    """检出「守卫绑定到变量」且「同作用域后续有 .await」的组合。

    判定「绑定到守卫」：该 let 语句以 .lock().unwrap() 之类结尾，且**不以**
    .clone() / 字段读取 / Copy 取值收尾（即整条链的结果仍是 MutexGuard）。
    """
    findings: list[Finding] = []
    notes: list[str] = []
    lines = code.split("\n")

    depth = 0
    # 栈：每层作用域内「当前存活的守卫绑定」
    scopes: list[list[GuardBinding]] = [[]]

    for idx, raw in enumerate(lines, start=1):
        line = raw
        opens = line.count("{")
        closes = line.count("}")

        # 先看本行是否绑定守卫（在处理括号变化前，按当前 depth 记账）
        m = BIND_GUARD_RE.match(line)
        if m and ".lock()" in line:
            rhs = m.group(2).rstrip()
            # 结果仍是守卫：整条链以 .lock() / .unwrap() / .expect(..) 收尾。
            # 注意必须匹配 unwrap 后面的空参数括号，否则会漏判（守卫被误认为 owned）。
            still_guard = bool(
                re.search(r"\.(?:unwrap\(\)|expect\([^)]*\)|lock\(\))\s*;?\s*$", rhs)
            )
            if still_guard:
                scopes[-1].append(GuardBinding(idx, line.strip(), depth))
            else:
                notes.append(f"{path}:{idx}: lock 链以 owned 结尾（安全）-> {line.strip()}")

        # 本行是否出现 .await
        if ".await" in line:
            live = [g for sc in scopes for g in sc]
            for g in live:
                findings.append(Finding(path, g, idx, line.strip()))

        depth += opens - closes
        # 处理作用域进出：简化为「有 { 则压栈，有 } 则弹栈」
        for _ in range(opens):
            scopes.append([])
        for _ in range(closes):
            if len(scopes) > 1:
                scopes.pop()

    return findings, notes


# --------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------
def main(argv: list[str]) -> int:
    paths = argv[1:]
    if not paths:
        print("用法: rs_static_check.py <file.rs> [...]", file=sys.stderr)
        return 2

    hard_errors: list[str] = []
    soft_warns: list[str] = []
    guard_findings: list[Finding] = []
    safe_notes: list[str] = []

    for path in paths:
        try:
            with open(path, "r", encoding="utf-8") as fh:
                src = fh.read()
        except OSError as exc:
            hard_errors.append(f"{path}: 读取失败 {exc}")
            continue

        code = strip_noncode(src)
        hard_errors.extend(check_delimiters(path, code))
        soft_warns.extend(check_unused_imports(path, code))
        f, n = scan_guard_across_await(path, code)
        guard_findings.extend(f)
        safe_notes.extend(n)

    print("=" * 72)
    print(f"[1/3] 分隔符配对：检查 {len(paths)} 个文件")
    if hard_errors:
        for e in hard_errors:
            print(f"  ERROR {e}")
    else:
        print("  OK - (){}[]  全部平衡")

    print(f"[2/3] 导入-使用交叉核对")
    if soft_warns:
        for w in soft_warns:
            print(f"  WARN  {w}")
    else:
        print("  OK - 无「导入但未使用」")

    print(f"[3/3] MutexGuard 跨 await 扫描")
    if guard_findings:
        for f in guard_findings:
            print(
                f"  RISK  {f.path}: 守卫绑定于第 {f.guard.line} 行"
                f" [{f.guard.text}] 在第 {f.await_line} 行仍存活"
                f" [{f.await_text}]"
            )
    else:
        print("  OK - 无守卫绑定跨越 .await")
    for n in safe_notes:
        print(f"  note  {n}")
    print("=" * 72)

    if hard_errors or guard_findings:
        print("RESULT: FAIL")
        return 1
    print("RESULT: PASS" + ("  (含 %d 条 warn)" % len(soft_warns) if soft_warns else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
