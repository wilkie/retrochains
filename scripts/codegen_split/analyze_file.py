#!/usr/bin/env python3
"""Read-only structural analysis of an arbitrary Rust source file.

Usage: analyze_file.py <path> [out.txt]

Reports every top-level item (fn/struct/enum/impl/trait/type/const/static/mod/
use) with line span + size, and for each impl block the methods inside it.
Brace-matching is string/char/comment aware enough for this codebase.
"""
import re
import sys
from pathlib import Path


def strip_for_braces(line: str) -> str:
    out = []
    i, n = 0, len(line)
    while i < n:
        c = line[i]
        if c == '/' and i + 1 < n and line[i + 1] == '/':
            break
        if c == '"':
            i += 1
            while i < n:
                if line[i] == '\\':
                    i += 2; continue
                if line[i] == '"':
                    break
                i += 1
            i += 1; continue
        if c == "'":
            m = re.match(r"'(\\.|[^'\\])'", line[i:])
            if m:
                i += m.end(); continue
            out.append(c); i += 1; continue
        out.append(c); i += 1
    return "".join(out)


def span_from(lines, start_idx):
    depth, seen = 0, False
    i = start_idx
    while i < len(lines):
        for ch in strip_for_braces(lines[i]):
            if ch == '{':
                depth += 1; seen = True
            elif ch == '}':
                depth -= 1
        if seen and depth == 0:
            return i
        i += 1
    return len(lines) - 1


def main():
    src = Path(sys.argv[1])
    out_path = Path(sys.argv[2]) if len(sys.argv) > 2 else None
    text = src.read_text().splitlines()
    n = len(text)

    item_re = re.compile(
        r'^(pub(\([^)]*\))? )?(unsafe )?(impl|fn|struct|enum|trait|type|mod|const|static|use)\b')
    method_re = re.compile(r'^    (pub(\([^)]*\))? )?(unsafe )?fn (\w+)')

    out = [f"TOTAL LINES: {n}", ""]
    counts = {}
    i = 0
    while i < n:
        m = item_re.match(text[i])
        if not m:
            i += 1
            continue
        kind = m.group(4)
        counts[kind] = counts.get(kind, 0) + 1
        lineno = i + 1
        if kind in ("type", "const", "static", "mod", "use"):
            out.append(f"[{kind}] L{lineno}: {text[i].strip()[:95]}")
            i += 1
            continue
        end = span_from(text, i)
        size = end - i + 1
        out.append(f"[{kind}] L{lineno}-{end+1} ({size}): {text[i].strip()[:95]}")
        if kind == "impl":
            j = i + 1
            while j < end:
                mm = method_re.match(text[j])
                if mm:
                    mend = span_from(text, j)
                    out.append(f"    fn {mm.group(4)}  L{j+1}-{mend+1} ({mend-j+1})")
                    j = mend + 1
                else:
                    j += 1
        i = end + 1

    out.append("")
    out.append("ITEM COUNTS: " + ", ".join(f"{k}={v}" for k, v in sorted(counts.items())))
    blob = "\n".join(out) + "\n"
    if out_path:
        out_path.write_text(blob)
        print(f"wrote {out_path} ({len(out)} lines); counts: {counts}")
    else:
        print(blob)


if __name__ == "__main__":
    main()
