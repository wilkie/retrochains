#!/usr/bin/env python3
"""Split the 3838-line `impl Parser` in crates/bcc/src/parse/mod.rs into
concern modules (parse/{core,types,decls,stmts,exprs}.rs), each holding its
own `impl Parser` block. PURE CODE MOVE.

Same model as the bcc codegen split: struct Parser, enum ParseError, the free
helper fns and the `#[cfg(test)] mod tests` stay in mod.rs; only impl methods
move. Each submodule is `use super::*;` + `impl super::Parser { ... }`. Bare
`fn` becomes `pub(crate) fn` so sibling-module impl blocks can call across
files; methods already `pub`/`pub(crate)` keep their visibility (so external
callers and tests are unaffected).
"""
import re
import sys
from pathlib import Path

SRC = Path("crates/bcc/src/parse/mod.rs")
OUTDIR = Path("crates/bcc/src/parse")
TARGET_IMPL = "impl Parser {"

CORE = {"new", "peek", "peek_n", "bump", "at_eof", "expect"}
TYPES = {"parse_type", "parse_type_name", "parse_struct_type", "parse_union_type",
         "parse_record_type", "parse_enum_decl", "parse_enum_body", "parse_typedef",
         "is_bare_record_def", "parse_bare_record_decl", "make_ptr_ty",
         "make_ptr_ty_seg"}
DECLS = {"parse_unit", "parse_global", "parse_function", "parse_initializer",
         "parse_param_list", "parse_declare", "finish_declare",
         "finish_declare_unsized", "rename_shadowed_local", "lookup_block_rename",
         "parse_func_ptr_declarator"}
STMTS = {"parse_stmt", "parse_while", "parse_do_while", "parse_for",
         "parse_for_clause_list", "parse_for_clause_expr", "parse_switch",
         "parse_if", "parse_branch"}
EXPRS = {"parse_expr_or_lvalue_assign", "parse_expr", "parse_conditional",
         "parse_logor", "parse_logand", "parse_bitor", "parse_bitxor",
         "parse_bitand", "parse_equality", "parse_relational", "parse_shift",
         "parse_additive", "parse_multiplicative", "expr_static_size",
         "is_type_name_after_lparen", "parse_unary", "left_assoc", "parse_atom",
         "parse_primary"}


def categorize(name):
    if name in CORE:
        return "core"
    if name in TYPES:
        return "types"
    if name in DECLS:
        return "decls"
    if name in STMTS:
        return "stmts"
    if name in EXPRS:
        return "exprs"
    if name.startswith("consume_cc_modifiers"):
        return "types"
    return None


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
    raise RuntimeError("unbalanced")


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
    lines = SRC.read_text().splitlines()
    n = len(lines)
    impl_start = next(i for i, l in enumerate(lines) if l.rstrip() == TARGET_IMPL)
    impl_end = span_from(lines, impl_start)
    method_re = re.compile(r'^    (pub(\([^)]*\))? )?fn (\w+)')

    methods, uncategorized = [], []
    i, prev = impl_start + 1, impl_start
    while i < impl_end:
        m = method_re.match(lines[i])
        if m:
            end = span_from(lines, i)
            name = m.group(3)
            mod = categorize(name)
            if mod is None:
                uncategorized.append(name)
            else:
                methods.append((name, attach_start(lines, i, prev), end, mod))
            prev = end
            i = end + 1
        else:
            i += 1

    if uncategorized:
        print("ABORT uncategorized:", ", ".join(uncategorized)); sys.exit(1)

    # coverage: ensure no non-trivial lines left between methods
    covered = set()
    for _, s, e, _ in methods:
        covered.update(range(s, e + 1))
    gaps = [k + 1 for k in range(impl_start + 1, impl_end)
            if k not in covered and lines[k].strip()
            and not lines[k].strip().startswith(("//", "#[", "/*", "*", "*/"))]
    groups = {}
    for name, s, e, mod in methods:
        groups.setdefault(mod, []).append((name, s, e))
    print(f"impl L{impl_start+1}-{impl_end+1}  methods={len(methods)}  gaps={len(gaps)}")
    for mod in sorted(groups):
        sz = sum(e - s + 1 for _, s, e in groups[mod])
        print(f"  {mod:7s} {len(groups[mod]):3d} methods {sz:6d} lines")
    if gaps:
        print("GAP lines:", gaps[:10])
    if dry:
        return
    if gaps:
        print("ABORT: gaps"); sys.exit(1)

    header = "use super::*;\n\nimpl Parser {\n"
    for mod in sorted(groups):
        body = []
        for _, s, e in sorted(groups[mod], key=lambda t: t[1]):
            seg = list(lines[s:e + 1])
            for k, ln in enumerate(seg):
                if ln.startswith(("    pub fn ", "    pub(crate) fn ")):
                    break
                if ln.startswith("    fn "):
                    seg[k] = "    pub(crate) " + ln[4:]
                    break
            body.extend(seg)
        (OUTDIR / f"parse_{mod}.rs").write_text(header + "\n".join(body) + "\n}\n")

    decls = ["// impl Parser is split across concern modules; each holds its",
             "// own `impl Parser` block. See scripts/codegen_split/."]
    decls += [f"mod parse_{mod};" for mod in sorted(groups)]
    new = lines[:impl_start] + decls + lines[impl_end + 1:]
    SRC.write_text("\n".join(new) + "\n")
    print(f"\nmod.rs {n} -> {len(new)} lines; wrote {len(groups)} parse_*.rs")


if __name__ == "__main__":
    main()
