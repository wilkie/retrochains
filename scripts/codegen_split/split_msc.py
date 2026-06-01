#!/usr/bin/env python3
"""Split crates/msc/src/lib.rs (single 9.8k-line module) into phase modules,
mirroring bcc's layout. PURE CODE MOVE.

Strategy (same insight as the bcc split): keep every type definition, impl
block, the two public fns (emit_dash_c, build_obj) and EmitError in lib.rs.
Move only the *private free functions* into submodules. Because submodules are
descendants of the crate root, they can read the crate root's private types
and struct fields with no visibility changes — so the ONLY edit to moved code
is bumping `fn` -> `pub(crate) fn` so sibling submodules can call each other.

A crate-root re-export hub (`pub(crate) use <mod>::*;`) lifts every moved fn
back into crate scope, so `use crate::*;` in each submodule resolves all
sibling fns, all types, and lib.rs's own `use obj::ObjBuilder` / std imports.

Verification of the result is delegated to `cargo build -p msc` + the msc
fixture suite. Run with --dry-run to preview grouping without writing.
"""
import re
import sys
from pathlib import Path

ROOT = Path("crates/msc/src")
SRC = ROOT / "lib.rs"

KEEP = {"emit_dash_c", "build_obj"}  # public API stays in lib.rs

# codegen submodules live under crates/msc/src/codegen/<name>.rs;
# lex/parse are flat crates/msc/src/<name>.rs.
CODEGEN_MODS = {"func", "expr", "assign", "statements", "cond", "calls", "constprop"}

LEX = {"tokenize", "apply_typedef_substitutions"}
PARSE_EXTRA = {"init_expr_has_matching_literal_leaf", "skip_decl_modifiers",
               "cond_from_expr", "pointee_size_of", "expr_from_stmt_value"}
EXPR = {"emit_expr_to_ax", "emit_load_param", "emit_load_local", "emit_binop",
        "emit_binop_right", "const_index_global", "bp_load", "bp_disp",
        "emit_imm_op", "emit_mem_op_at"}
STATEMENTS = {"emit_stmt", "emit_return", "emit_while", "emit_for", "emit_loop",
              "emit_do_while", "emit_runtime_switch",
              "emit_partial_switch_with_continuation", "stmt_always_returns",
              "for_entry_fold", "fold_cond", "fold_cond_raw",
              "collect_loop_body_mutations", "body_sets_flags_for_cond",
              "expr_references_local", "cond_references_local",
              "cond_has_literal_side"}
FUNC = {"emit_function", "symbol_name", "bp_modrm", "push_bp_disp",
        "param_disp", "body_needs_si"}
CALLS = {"emit_call", "emit_call_inner", "emit_long_to_dx_ax"}


def categorize(name):
    if name in KEEP:
        return None
    if name in LEX:
        return ("flat", "lex")
    if name.startswith("parse_") or name in PARSE_EXTRA:
        return ("flat", "parse")
    if name.startswith("prop_") or name.startswith("const_prop") or name == "cp_clone":
        return ("cg", "constprop")
    if name.startswith("emit_assign") or name.startswith("emit_postmutate") \
            or name in {"emit_index_to_si", "emit_byte_rhs_to_al"}:
        return ("cg", "assign")
    if name.startswith("emit_cond") or name.startswith("emit_cmp") \
            or name in {"inverted_jcc", "loop_back_jcc"}:
        return ("cg", "cond")
    if name.startswith("emit_push_arg") or name in CALLS:
        return ("cg", "calls")
    if name in EXPR:
        return ("cg", "expr")
    if name in STATEMENTS:
        return ("cg", "statements")
    if name in FUNC:
        return ("cg", "func")
    return ("UNCATEGORIZED", name)


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
        # a one-line fn ending in ; with no body (none here) — guard:
        if lines[i].rstrip().endswith(';') and not seen and i == start:
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
    lines = SRC.read_text().splitlines()
    n = len(lines)
    fn_re = re.compile(r'^(pub(\([^)]*\))? )?fn (\w+)')

    moved = []  # (name, start, end, target)
    uncategorized = []
    i = 0
    prev_end = -1
    while i < n:
        m = fn_re.match(lines[i])
        if m:
            name = m.group(3)
            end = span_from(lines, i)
            cat = categorize(name)
            if cat is not None:
                kind, where = cat
                if kind == "UNCATEGORIZED":
                    uncategorized.append(name)
                else:
                    start = attach_start(lines, i, prev_end)
                    moved.append((name, start, end, (kind, where)))
            prev_end = end
            i = end + 1
        else:
            i += 1

    if uncategorized:
        print("ABORT: uncategorized free fns:", ", ".join(uncategorized))
        sys.exit(1)

    # group
    groups = {}
    for name, s, e, tgt in moved:
        groups.setdefault(tgt, []).append((name, s, e))

    print(f"moving {len(moved)} free fns into {len(groups)} modules:")
    for tgt in sorted(groups):
        sz = sum(e - s + 1 for _, s, e in groups[tgt])
        names = ", ".join(nm for nm, _, _ in groups[tgt])
        print(f"  {tgt[0]}/{tgt[1]:12s} {len(groups[tgt]):3d} fns {sz:6d} lines")
        if dry:
            print(f"      {names[:240]}")
    if dry:
        return

    # Build set of moved line indices for removal.
    moved_idx = set()
    for _, s, e, _ in moved:
        moved_idx.update(range(s, e + 1))

    # Write module files (functions in original source order per module).
    def render(fns):
        body = []
        for _, s, e in sorted(fns, key=lambda t: t[1]):
            seg = list(lines[s:e + 1])
            for k, ln in enumerate(seg):
                if ln.startswith("pub(crate) fn ") or ln.startswith("pub fn "):
                    break
                if ln.startswith("fn "):
                    seg[k] = "pub(crate) " + ln
                    break
            body.extend(seg)
        return "use crate::*;\n\n" + "\n".join(body) + "\n"

    flat_mods, cg_mods = [], []
    (ROOT / "codegen").mkdir(exist_ok=True)
    for tgt in sorted(groups):
        kind, where = tgt
        content = render(groups[tgt])
        if kind == "flat":
            (ROOT / f"{where}.rs").write_text(content)
            flat_mods.append(where)
        else:
            (ROOT / "codegen" / f"{where}.rs").write_text(content)
            cg_mods.append(where)

    # codegen/mod.rs: declare + re-export children.
    cg_lines = ["//! MSC code generation, split by concern. Type definitions",
                "//! live in the crate root (`lib.rs`); these modules hold the",
                "//! emission free functions.", ""]
    for w in sorted(cg_mods):
        cg_lines.append(f"mod {w};")
    cg_lines.append("")
    for w in sorted(cg_mods):
        cg_lines.append(f"pub(crate) use {w}::*;")
    (ROOT / "codegen" / "mod.rs").write_text("\n".join(cg_lines) + "\n")

    # Rewrite lib.rs: drop moved fns, insert module decls + re-export hub
    # right after the leading `use` block (after `use obj::ObjBuilder;`).
    new = [ln for idx, ln in enumerate(lines) if idx not in moved_idx]
    insert_at = next(i for i, ln in enumerate(new)
                     if ln.startswith("use obj::ObjBuilder")) + 1
    hub = [""]
    for w in sorted(flat_mods):
        hub.append(f"mod {w};")
    hub.append("mod codegen;")
    hub.append("")
    hub.append("// Phase modules hold the free functions; types/impls stay here.")
    for w in sorted(flat_mods):
        hub.append(f"pub(crate) use {w}::*;")
    hub.append("pub(crate) use codegen::*;")
    new[insert_at:insert_at] = hub
    SRC.write_text("\n".join(new) + "\n")
    print(f"\nrewrote lib.rs: {n} -> {len(new)} lines; "
          f"wrote {len(flat_mods)} flat + {len(cg_mods)} codegen modules.")


if __name__ == "__main__":
    main()
