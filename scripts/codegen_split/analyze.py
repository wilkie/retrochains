#!/usr/bin/env python3
"""Read-only structural analysis of crates/bcc/src/codegen/mod.rs.

Parses the file with brace matching (string/char/comment aware enough for
this codebase) and reports every top-level item: free fns, structs, enums,
impl blocks, and the methods inside each impl with exact line spans.

Output is written to scripts/codegen_split/map.txt so it can be read in
chunks without relying on terminal rendering.
"""
import re
import sys
from pathlib import Path

SRC = Path("crates/bcc/src/codegen/mod.rs")


def strip_for_braces(line: str) -> str:
    """Remove // line comments, string and char literals so brace counting
    isn't fooled by braces inside them. Not a full Rust lexer, but adequate
    for balanced-brace span detection in this file."""
    out = []
    i = 0
    n = len(line)
    while i < n:
        c = line[i]
        if c == '/' and i + 1 < n and line[i + 1] == '/':
            break
        if c == '"':
            i += 1
            while i < n:
                if line[i] == '\\':
                    i += 2
                    continue
                if line[i] == '"':
                    break
                i += 1
            i += 1
            continue
        if c == "'":
            # char literal or lifetime; only treat as literal if it closes soon
            m = re.match(r"'(\\.|[^'\\])'", line[i:])
            if m:
                i += m.end()
                continue
            out.append(c)
            i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def span_from(lines, start_idx):
    """Given a 0-based index of a line that contains an opening brace for an
    item, return the 0-based index of the line holding the matching close.
    Handles the opening brace appearing on a later line than start_idx."""
    depth = 0
    seen_open = False
    i = start_idx
    while i < len(lines):
        s = strip_for_braces(lines[i])
        for ch in s:
            if ch == '{':
                depth += 1
                seen_open = True
            elif ch == '}':
                depth -= 1
        if seen_open and depth == 0:
            return i
        i += 1
    return len(lines) - 1


def main():
    text = SRC.read_text().splitlines()
    n = len(text)

    item_re = re.compile(r'^(pub(\([^)]*\))? )?(unsafe )?(impl|fn|struct|enum|trait|type|mod|const|static)\b')
    method_re = re.compile(r'^    (pub(\([^)]*\))? )?(unsafe )?fn (\w+)')

    out = []
    out.append(f"TOTAL LINES: {n}")
    out.append("")

    i = 0
    while i < n:
        line = text[i]
        m = item_re.match(line)
        if not m:
            i += 1
            continue
        kind = m.group(4)
        lineno = i + 1  # 1-based
        if kind in ("type", "const", "static", "mod"):
            out.append(f"[{kind}] L{lineno}: {line.strip()[:90]}")
            i += 1
            continue
        # block items: find span
        end = span_from(text, i)
        endno = end + 1
        size = endno - lineno + 1
        head = line.strip()[:90]
        out.append(f"[{kind}] L{lineno}-{endno} ({size} lines): {head}")
        if kind == "impl":
            # enumerate methods at one indent level inside
            j = i + 1
            while j < end:
                mm = method_re.match(text[j])
                if mm:
                    name = mm.group(4)
                    mend = span_from(text, j)
                    msize = mend - j + 1
                    out.append(f"    fn {name}  L{j+1}-{mend+1}  ({msize})")
                    j = mend + 1
                else:
                    j += 1
        i = end + 1

    Path("scripts/codegen_split/map.txt").write_text("\n".join(out) + "\n")
    print(f"wrote scripts/codegen_split/map.txt ({len(out)} lines)")


if __name__ == "__main__":
    main()
