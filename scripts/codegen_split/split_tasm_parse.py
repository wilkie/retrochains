#!/usr/bin/env python3
"""Split crates/bcc-tasm/src/parse.rs (4943 lines) into a parse/ directory of
concern modules. PURE CODE MOVE.

The file is mostly free functions (instruction + operand parsers). Strategy
mirrors the msc split but for a non-crate-root module: keep `pub fn parse`,
struct Parser + its impl, the Line Copy/Clone impls, enum BpWidth and the
`#[cfg(test)] mod tests` in parse/mod.rs; move the other free fns into
submodules. Each submodule is `use super::*;` (super = the parse module,
which carries the `use crate::ir::...` imports and a re-export hub). Bare
`fn` becomes `pub(crate) fn`.

Converts the flat parse.rs into parse/mod.rs + parse/<mod>.rs (removes the
old flat file).
"""
import os
import re
import sys
from pathlib import Path

SRCFILE = Path("crates/bcc-tasm/src/parse.rs")
DESTDIR = Path("crates/bcc-tasm/src/parse")
KEEP = {"parse"}  # public entry fn stays in mod.rs

INSTR = {"parse_instr", "parse_segment_attrs"}
ALU_MOV = {"parse_mov", "parse_single_op_word_ptr", "parse_alu_ax_mem"}
ALU_ARITH = {"parse_add", "parse_sub", "parse_adc", "parse_sbb", "parse_cmp"}
ALU_LOGIC = {"parse_and", "parse_or", "parse_xor"}
SHIFTS = {"parse_jmp", "parse_jmp_cond", "parse_lea", "parse_shl_one",
          "parse_rcl_one", "parse_sar_one", "parse_shr_one", "parse_rcr_one"}
FPU = {"parse_fld", "parse_fstp", "parse_fpu_arith"}


def categorize(name):
    if name in KEEP:
        return None
    if name in INSTR:
        return "instr"
    if name in ALU_MOV:
        return "alu_mov"
    if name in ALU_ARITH:
        return "alu_arith"
    if name in ALU_LOGIC:
        return "alu_logic"
    if name in SHIFTS:
        return "shifts"
    if name in FPU:
        return "fpu"
    return "operands"  # all the small operand/immediate helpers


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
    # top-level free fn (column 0)
    fn_re = re.compile(r'^(pub(\([^)]*\))? )?fn (\w+)')

    moved = []
    i, prev = 0, -1
    while i < n:
        m = fn_re.match(lines[i])
        if m:
            end = span_from(lines, i)
            name = m.group(3)
            mod = categorize(name)
            if mod is not None:
                moved.append((name, attach_start(lines, i, prev), end, mod))
            prev = end
            i = end + 1
        else:
            i += 1

    groups = {}
    for name, s, e, mod in moved:
        groups.setdefault(mod, []).append((name, s, e))
    print(f"moving {len(moved)} free fns into {len(groups)} modules:")
    for mod in sorted(groups):
        sz = sum(e - s + 1 for _, s, e in groups[mod])
        flag = "  <-- OVER 3k" if sz > 3000 else ""
        print(f"  {mod:10s} {len(groups[mod]):3d} fns {sz:6d} lines{flag}")
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

    # facade = remaining lines + module decls + re-export hub, inserted after
    # the leading `use` block.
    new = [ln for idx, ln in enumerate(lines) if idx not in moved_idx]
    insert_at = max(i for i, ln in enumerate(new)
                    if ln.startswith("use ")) + 1
    hub = [""]
    hub += [f"mod {mod};" for mod in sorted(groups)]
    hub += [""]
    hub += [f"pub(crate) use {mod}::*;" for mod in sorted(groups)]
    new[insert_at:insert_at] = hub

    os.remove(SRCFILE)
    (DESTDIR / "mod.rs").write_text("\n".join(new) + "\n")
    print(f"\nparse.rs {n} -> parse/mod.rs {len(new)} lines; "
          f"wrote {len(groups)} submodules; removed flat parse.rs")


if __name__ == "__main__":
    main()
