#!/usr/bin/env python3
"""Split crates/bcc-tasm/src/encode.rs (4409 lines) into an encode/ directory.
PURE CODE MOVE.

The file is free functions + a few structs. The orchestration entry
(encode_module) and its small build_* helpers, the public structs/type
aliases, and the `#[cfg(test)] mod tests` stay in encode/mod.rs. The rest
moves out: encode_segment -> segment.rs, instr_size -> size.rs, the giant
3283-line emit_instr -> emit_instr.rs (kept whole: it is one cohesive
dispatch match; breaking the match would be a behavioural refactor, not a
move -- at 3283 lines it sits under the 4k ceiling like locals.rs), and the
small emit_*/push_* helpers -> helpers.rs.

Each submodule is `use super::*;` + the crate-root re-export hub in mod.rs.
Bare `fn` becomes `pub(crate) fn`. Converts flat encode.rs into a directory.
"""
import os
import re
import sys
from pathlib import Path

SRCFILE = Path("crates/bcc-tasm/src/encode.rs")
DESTDIR = Path("crates/bcc-tasm/src/encode")
KEEP = {"encode_module", "build_extern_idx", "build_symbols",
        "build_group_idx", "build_segment_idx"}


def categorize(name):
    if name in KEEP:
        return None
    if name == "encode_segment":
        return "segment"
    if name == "instr_size":
        return "size"
    if name == "emit_instr":
        return "emit_instr"
    return "helpers"  # push_*_fixup, emit_bp_rel_modrm, emit_group_sym_*, ...


def strip_for_braces(line):
    out, i, n = [], 0, len(line)
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


def span_from(lines, start):
    depth, seen, i = 0, False, start
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


def attach_start(lines, fn_idx, lower):
    s = fn_idx
    while s - 1 > lower:
        t = lines[s - 1].strip()
        if t.startswith(("#[", "#![", "///", "//!", "//", "/*", "*", "*/")):
            s -= 1
        else:
            break
    return s


def main():
    dry = "--dry-run" in sys.argv
    lines = SRCFILE.read_text().splitlines()
    n = len(lines)
    fn_re = re.compile(r'^(pub(\([^)]*\))? )?fn (\w+)')

    moved = []
    i, prev = 0, -1
    while i < n:
        m = fn_re.match(lines[i])
        if m:
            end = span_from(lines, i)
            mod = categorize(m.group(3))
            if mod is not None:
                moved.append((m.group(3), attach_start(lines, i, prev), end, mod))
            prev = end
            i = end + 1
        else:
            i += 1

    groups = {}
    for name, s, e, mod in moved:
        groups.setdefault(mod, []).append((name, s, e))
    print(f"moving {len(moved)} fns into {len(groups)} modules:")
    for mod in sorted(groups):
        sz = sum(e - s + 1 for _, s, e in groups[mod])
        flag = "  <-- over 4k CEILING" if sz > 4000 else (
            "  (over 3k target, under ceiling)" if sz > 3000 else "")
        print(f"  {mod:11s} {len(groups[mod]):2d} fns {sz:6d} lines{flag}")
    if dry:
        return

    moved_idx = set()
    for _, s, e, _ in moved:
        moved_idx.update(range(s, e + 1))

    DESTDIR.mkdir(exist_ok=True)
    for mod in sorted(groups):
        body = []
        for _, s, e in sorted(groups[mod], key=lambda t: t[1]):
            seg = list(lines[s:e + 1])
            for k, ln in enumerate(seg):
                if ln.startswith(("pub fn ", "pub(crate) fn ")):
                    break
                if ln.startswith("fn "):
                    seg[k] = "pub(crate) " + ln
                    break
            body.extend(seg)
        (DESTDIR / f"{mod}.rs").write_text("use super::*;\n\n" + "\n".join(body) + "\n")

    new = [ln for idx, ln in enumerate(lines) if idx not in moved_idx]
    # Insert the hub before the first top-level item definition. (Anchoring on
    # the last `use ` line is unsafe: a multi-line `use foo::{ ... };` would
    # put the hub inside the braces.)
    item_re = re.compile(r'^(pub(\([^)]*\))? )?(type|struct|enum|fn|impl|const|static)\b')
    insert_at = next(i for i, ln in enumerate(new) if item_re.match(ln))
    # back up over the item's leading doc-comment/attr block so the hub goes
    # above it (keeps the doc comment attached to its item, not to `mod ...`)
    while insert_at > 0 and new[insert_at - 1].strip().startswith(
            ("///", "//!", "//", "#[", "/*", "*", "*/")):
        insert_at -= 1
    hub = [""] + [f"mod {m};" for m in sorted(groups)] + [""] \
        + [f"pub(crate) use {m}::*;" for m in sorted(groups)]
    new[insert_at:insert_at] = hub

    os.remove(SRCFILE)
    (DESTDIR / "mod.rs").write_text("\n".join(new) + "\n")
    print(f"\nencode.rs {n} -> encode/mod.rs {len(new)} lines; "
          f"wrote {len(groups)} submodules; removed flat encode.rs")


if __name__ == "__main__":
    main()
