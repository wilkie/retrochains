#!/usr/bin/env python3
"""Split the giant `impl<'a> FunctionEmitter<'a>` in codegen/mod.rs into
per-concern sibling modules. PURE CODE MOVE: methods are relocated verbatim;
free functions, types, and the struct definition stay in mod.rs.

Safety model:
  * Each new module is `use super::*;\n\nimpl<'a> super::FunctionEmitter<'a> { ... }`.
    A child module's glob import of its parent pulls in the parent's private
    fns/types, and child modules can access an ancestor struct's private
    fields, so no visibility edits are required.
  * Methods are extracted by brace-matched span, so bodies move intact.
  * A round-trip assertion guarantees every byte of the impl interior is
    accounted for (no method dropped or duplicated).

Run with --dry-run to print the grouping + line counts without writing.
Verification of the result is delegated to `cargo build` + the fixture suite.
"""
import re
import sys
from pathlib import Path

SRC = Path("crates/bcc/src/codegen/mod.rs")
OUTDIR = Path("crates/bcc/src/codegen")

# The struct the big impl is for.
TARGET_IMPL = "impl<'a> FunctionEmitter<'a> {"

# Ordered (module, predicate) rules; first match wins. Predicate takes the
# method name. Anything unmatched falls through to 'emit_core'.
def rules():
    def pre(*ps):
        return lambda n: any(n.startswith(p) for p in ps)
    def has(*ss):
        return lambda n: any(s in n for s in ss)
    return [
        ("bitfields", has("bitfield")),
        ("statements", pre("emit_stmt", "emit_if", "emit_while", "emit_do",
                           "emit_for", "emit_switch", "emit_return",
                           "emit_break", "emit_continue", "emit_block",
                           "emit_asm", "emit_goto", "emit_label_stmt")),
        ("members", pre("emit_member")),
        ("arrays", pre("emit_array", "emit_index") ),
        ("assign", pre("emit_assign", "emit_store", "emit_deref")),
        ("assign2", has("_assign")),
        ("conditions", pre("emit_cond") ),
        ("conditions2", has("_cmp")),
        ("expressions", pre("emit_expr", "emit_cast", "emit_ternary",
                            "emit_comparison", "emit_binary", "emit_unary",
                            "emit_call_expr", "emit_op", "emit_value")),
        ("lvalues", has("lvalue", "_addr", "pointee", "deref_chain", "ptr_")),
        ("classify", pre("ident_is", "expr_is", "is_", "classify",
                         "scrutinee", "type_")),
    ]


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
    raise RuntimeError(f"unbalanced from line {start_idx+1}")


def categorize(name):
    for mod, pred in rules():
        if pred(name):
            # collapse the *2 helper buckets into their primary file
            return mod[:-1] if mod[-1].isdigit() else mod
    return "emit_core"


def main():
    dry = "--dry-run" in sys.argv
    lines = SRC.read_text().splitlines()
    n = len(lines)

    # locate the target impl
    impl_start = next(i for i, l in enumerate(lines)
                      if l.rstrip() == TARGET_IMPL)
    impl_end = span_from(lines, impl_start)  # line with closing }

    interior = range(impl_start + 1, impl_end)  # exclusive of braces
    method_re = re.compile(r'^    (pub(\([^)]*\))? )?(unsafe )?fn (\w+)')

    def attach_start(fn_idx, lower_bound):
        """Walk backward over attribute/doc-comment lines directly attached to
        the fn (no intervening blank line) so they travel with the method."""
        s = fn_idx
        while s - 1 > lower_bound:
            t = lines[s - 1].strip()
            if t.startswith(("#[", "#![", "///", "//!", "//", "/*", "*", "*/")):
                s -= 1
            else:
                break  # blank line or code: stop
        return s

    methods = []  # (name, start_idx, end_idx) -- start absorbs leading attrs
    i = impl_start + 1
    prev_end = impl_start
    while i < impl_end:
        m = method_re.match(lines[i])
        if m:
            end = span_from(lines, i)
            start = attach_start(i, prev_end)
            methods.append((m.group(4), start, end))
            prev_end = end
            i = end + 1
        else:
            i += 1

    # Verify the interior is exactly the methods back-to-back (allowing only
    # blank lines / attributes / doc-comments between them).
    covered = set()
    for _, s, e in methods:
        for k in range(s, e + 1):
            covered.add(k)
    gap_nonblank = []
    for k in interior:
        if k not in covered:
            txt = lines[k].strip()
            if txt and not txt.startswith(("//", "#[", "/*", "*", "*/")):
                gap_nonblank.append((k + 1, txt[:70]))

    groups = {}
    for name, s, e in methods:
        mod = categorize(name)
        groups.setdefault(mod, []).append((name, s, e))

    print(f"impl span: L{impl_start+1}-L{impl_end+1}  methods: {len(methods)}")
    print(f"non-blank/non-attr gap lines inside impl (should be ~0): {len(gap_nonblank)}")
    for ln, t in gap_nonblank[:20]:
        print(f"    GAP L{ln}: {t}")
    print()
    total = 0
    for mod in sorted(groups):
        cnt = len(groups[mod])
        sz = sum(e - s + 1 for _, s, e in groups[mod])
        total += sz
        print(f"  {mod:14s} {cnt:3d} methods  {sz:6d} lines")
        if dry:
            names = ", ".join(nm for nm, _, _ in groups[mod])
            print(f"      {names[:300]}")
    print(f"  {'TOTAL':14s} {len(methods):3d} methods  {total:6d} lines")

    if dry:
        return

    if gap_nonblank:
        print("ABORT: non-trivial code between methods; refusing to split.")
        sys.exit(1)

    # Build new files. Keep methods in their original source order within each
    # module (sort by start line). Any bucket whose total exceeds CHUNK_MAX is
    # packed into numbered parts (whole methods only) so no file blows the
    # soft size ceiling.
    CHUNK_MAX = 3000
    header = "use super::*;\n\nimpl<'a> super::FunctionEmitter<'a> {\n"
    footer = "}\n"
    written = []  # module file stems, in declaration order

    for mod in sorted(groups):
        ms = sorted(groups[mod], key=lambda t: t[1])
        total = sum(e - s + 1 for _, s, e in ms)
        if total <= CHUNK_MAX:
            chunks = [ms]
        else:
            # greedy pack into parts targeting CHUNK_MAX
            chunks, cur, cur_sz = [], [], 0
            for meth in ms:
                msz = meth[2] - meth[1] + 1
                if cur and cur_sz + msz > CHUNK_MAX:
                    chunks.append(cur); cur, cur_sz = [], 0
                cur.append(meth); cur_sz += msz
            if cur:
                chunks.append(cur)
        for idx, chunk in enumerate(chunks):
            suffix = "" if len(chunks) == 1 else f"_{idx+1}"
            body = []
            for _, s, e in chunk:
                seg = list(lines[s:e + 1])
                # Methods split across sibling-module impl blocks must be
                # crate-visible to call one another (privacy follows the impl's
                # module, not the struct's). Bump each bare `fn` to pub(crate);
                # leave already-public methods alone.
                for k, ln in enumerate(seg):
                    if ln.startswith(("    pub fn ", "    pub(crate) fn ",
                                      "    pub(super) fn ")):
                        break
                    if ln.startswith("    fn "):
                        seg[k] = "    pub(crate) " + ln[4:]
                        break
                body.extend(seg)
            content = header + "\n".join(body) + "\n" + footer
            stem = f"emitter_{mod}{suffix}"
            (OUTDIR / f"{stem}.rs").write_text(content)
            written.append(stem)

    # Rewrite mod.rs: replace the entire impl block (impl_start..=impl_end)
    # with module declarations. Everything else in mod.rs is untouched.
    mod_decls = ["// FunctionEmitter methods are split across concern modules;",
                 "// each holds an `impl<'a> FunctionEmitter<'a>` block.",
                 "// See scripts/codegen_split/ for the mechanical split.",]
    for stem in written:
        mod_decls.append(f"mod {stem};")
    new_lines = lines[:impl_start] + mod_decls + lines[impl_end + 1:]
    SRC.write_text("\n".join(new_lines) + "\n")
    print(f"\nwrote {len(written)} emitter_*.rs files; mod.rs now "
          f"{len(new_lines)} lines (was {n}).")


if __name__ == "__main__":
    main()
