#!/usr/bin/env python3
"""Propose a 2-level (area / sub-feature) category for every fixture.

The corpus is being reorganized from a flat `fixtures/<lang>/NNN-slug/` layout
into nested `fixtures/<lang>/<area>/<sub>/NNN-slug/` directories so gaps in
coverage are easier to spot. Global per-language numbering is preserved (the
number stays in the directory name); only the parent path changes.

Classification is rule-based over the descriptive slug (the dir name minus the
`NNN-` prefix and any trailing `-obj`). RULES is an ordered table; the FIRST
matching rule wins, so order encodes priority (most-specific construct first:
preprocessor > control-flow > aggregates > arrays > pointers > functions >
library/intrinsics > floating-point > expressions > types). Tokens are matched
against the hyphen-padded slug so `-for-` etc. are whole-token matches.

Usage:
    scripts/categorize_fixtures.py                 # write mapping CSV to stdout
    scripts/categorize_fixtures.py --summary       # bucket counts to stderr only
    scripts/categorize_fixtures.py --emit-moves    # print `git mv` commands
    scripts/categorize_fixtures.py --lang c        # restrict to one language

The mapping is meant to be reviewed (and the rules tweaked) before any moves.
"""
from __future__ import annotations
import argparse
import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "fixtures"

# Already-curated category dirs are folded into the new scheme rather than
# re-classified from their slug. Maps an existing top-level category dir name
# to its (area, sub) home.
EXISTING_CATEGORY = {
    "bitfields": ("aggregates", "bitfields"),
    "loops": ("control-flow", "loops"),
    "register-allocation": ("codegen", "register-allocation"),
    "symbol-ordering": ("codegen", "symbol-ordering"),
}

# (area, sub, regex-over-padded-slug). First match wins.
RULES: list[tuple[str, str, str]] = [
    # ---- preprocessor -----------------------------------------------------
    ("preprocessor", "macros",      r"-(macro|define|defined|undef|expand|expansion)-"),
    ("preprocessor", "token-ops",   r"-(paste|stringize|stringise|token|concat-op)-"),
    ("preprocessor", "includes",    r"-(include|header)-"),
    ("preprocessor", "conditional", r"-(ifdef|ifndef|elif|cond-compil|if-defined)-"),
    ("preprocessor", "directives",  r"-(pragma|line-directive|line-dir|error-directive)-"),
    # ---- control-flow -----------------------------------------------------
    ("control-flow", "switch",      r"-(switch|case|jump-table|jumptable|dense|sparse|duff|default-case)-"),
    ("control-flow", "loops",       r"-(while|for|do-while|dowhile|loop|iterate|do-loop)-"),
    ("control-flow", "jumps",       r"-(goto|label|break|continue)-"),
    ("control-flow", "conditionals",r"-(if|else|elseif|ternary|cond|select-expr)-"),
    # ---- aggregates -------------------------------------------------------
    ("aggregates", "bitfields",     r"-(bitfield|bit-field)-"),
    ("aggregates", "union",         r"-(union)-"),
    ("aggregates", "struct",        r"-(struct|field|member)-"),
    # ---- arrays -----------------------------------------------------------
    ("arrays", "multidim",          r"-(2d|multidim|multi-dim|matrix|nested-arr|arr-of-arr|array-of-arr)-"),
    ("arrays", "of-pointers",       r"-(arr-of-ptr|array-of-ptr|arr-of-string|arr-of-strings)-"),
    ("arrays", "strings",           r"-(string|str-lit|strlit|escape|char-array)-"),
    ("arrays", "init",              r"-(arr-init|array-init|arr-of|init-arr)-"),
    ("arrays", "indexing",          r"-(arr|array|elem|element|index|subscript)-"),
    # ---- pointers (memory model first, then access pattern) ---------------
    ("pointers", "segment",         r"-(seg|segment|_ss|_es|_cs|_ds|seg-qual)-"),
    ("pointers", "huge",            r"-(huge)-"),
    ("pointers", "far",             r"-(far)-"),
    ("pointers", "near",            r"-(near|compact)-"),
    ("pointers", "address-of",      r"-(addr|address-of|addressof|amp)-"),
    ("pointers", "arithmetic",      r"-(ptr-add|ptr-sub|ptr-inc|ptr-incr|stride|ptr-arith)-"),
    ("pointers", "deref",           r"-(deref|indirect|ptr|pointer)-"),
    # ---- functions --------------------------------------------------------
    ("functions", "varargs",        r"-(vararg|varargs|ellipsis|va-arg|va-list)-"),
    ("functions", "recursion",      r"-(recurs|recursive|fib|factorial|ackermann)-"),
    ("functions", "signatures",     r"-(knr|prototype|proto|forward-decl|signature|implicit-decl|implicit)-"),
    ("functions", "args",           r"-(arg|args|param|parameter|actual|formal|callee-arg)-"),
    ("functions", "return",         r"-(return|ret|retval)-"),
    ("functions", "function-pointers", r"-(fnptr|funcptr|func-ptr|function-ptr|function-pointer)-"),
    ("functions", "calls",          r"-(call|invoke|callee|caller|fn)-"),
    # ---- library (libc/runtime) ------------------------------------------
    ("library", "stdio",            r"-(printf|puts|putchar|getchar|scanf|gets|fputs|sprintf|puts-)-"),
    ("library", "string-fns",       r"-(strlen|strcpy|strcat|strcmp|strncpy|memcpy|memset|memcmp)-"),
    ("library", "alloc",            r"-(malloc|calloc|realloc|free|alloca|stack-alloc|large-stack)-"),
    ("library", "math",             r"-(abs|labs|fabs)-"),
    # ---- intrinsics (Borland low-level) ----------------------------------
    ("intrinsics", "port-io",       r"-(inp|outp|inport|outport|inportb|outportb)-"),
    ("intrinsics", "mem-access",    r"-(peek|poke|peekb|pokeb|movedata|mem-poke)-"),
    ("intrinsics", "dos-int",       r"-(int86|intdos|intr|geninterrupt|bdos|dos-int)-"),
    ("intrinsics", "pseudo-registers", r"-(pseudo|_ax|_al|_bx|_cx|_dx|_flags|_si|_di)-"),
    ("intrinsics", "inline-asm",    r"-(asm|inline-asm)-"),
    # ---- floating-point (before generic expressions) ---------------------
    ("floating-point", "conversion",r"-(float|double|fp).*(promote|convert|to-int|to-double|to-float|cast)-|-(promote|to-double|to-float).*-"),
    ("floating-point", "varargs",   r"-(percent-f|pct-f|printf-f|f-vararg)-"),
    ("floating-point", "arithmetic",r"-(float|double|fp)-.*(add|sub|mul|div)|-(fadd|fsub|fmul|fdiv)-"),
    ("floating-point", "compare",   r"-(float|double|fp)-.*(cmp|eq|lt|gt|le|ge|ne)"),
    ("floating-point", "scalar",    r"-(float|double|fp)-"),
    # ---- expressions ------------------------------------------------------
    ("expressions", "compound-assign", r"-(compound|comp-assign|incr-longhand|assign-incr|(add|sub|mul|div|mod|and|or|xor|shl|shr)eq|(plus|minus|times|div|mod)-eq)-"),
    ("expressions", "inc-dec",      r"-(postinc|preinc|postdec|predec|post-inc|pre-inc|post-dec|pre-dec|incr|decr)-"),
    ("expressions", "bitwise",      r"-(and|or|xor|not|shl|shr|ushr|sshr|shift|rot|rotate|complement|onescomp|bit-and|bit-or|bitand|bitor|bit-clear|bit-set|bit-test|bit-count|popcount)-"),
    ("expressions", "compare",      r"-(cmp|compare|eq|ne|neq|lt|gt|ge|le|less|greater|equal|relational)-"),
    ("expressions", "logical",      r"-(logical|land|lor|short-circuit|bool|double-not|notnot|bang)-"),
    ("expressions", "arithmetic",   r"-(add|sub|mul|div|mod|plus|minus|neg|negate|inc|dec|sum|imul|idiv|smul|sdiv|umul|udiv|max|min)-"),
    ("expressions", "cast",         r"-(cast|truncate|widen|sign-extend|zero-extend|sext|zext|convert|conv)-"),
    ("expressions", "sizeof",       r"-(sizeof)-"),
    ("expressions", "const-fold",   r"-(const-fold|cfold|const-expr|constant-fold|fold|literal-fold|paren-redundant|paren)-"),
    ("expressions", "comma",        r"-(comma|sequence-point|seq-expr)-"),
    ("expressions", "assignment",   r"-(assign|passthrough|self-assign|copy-prop|swap)-"),
    ("expressions", "ternary",      r"-(ternary)-"),
    # ---- types (declaration & storage; no operation matched above) -------
    ("types", "enum",               r"-(enum)-"),
    ("types", "storage-class",      r"-(static|extern|register|auto|volatile|const)-"),
    ("types", "initialization",     r"-(init|initializer|uninit|default-init|init-expr|constants?)-"),
    ("types", "globals",            r"-(global|globals)-"),
    ("types", "locals",             r"-(local|locals|frame|spill|shadowing|stack-slot|temp)-"),
    ("types", "long",               r"-(long|ulong)-"),
    ("types", "char",               r"-(char|uchar)-"),
    ("types", "integer",            r"-(int|uint|unsigned|short|word|byte)-"),
]


def slug_of(dirname: str) -> str:
    s = re.sub(r"^\d+-", "", dirname)
    s = re.sub(r"-obj$", "", s)
    return s


def classify(slug: str) -> tuple[str, str]:
    pad = f"-{slug}-"
    for area, sub, pat in RULES:
        if re.search(pat, pad):
            return area, sub
    return "misc", "unsorted"


def is_fixture(d: Path) -> bool:
    return any(d.glob("invocation.*.toml"))


def discover(lang: str | None):
    """Yield (lang, fixture_dir, existing_category_or_None)."""
    langs = [lang] if lang else [p.name for p in FIXTURES.iterdir() if p.is_dir()]
    for lg in langs:
        base = FIXTURES / lg
        if not base.is_dir():
            continue
        for sub in sorted(base.iterdir()):
            if not sub.is_dir():
                continue
            if is_fixture(sub):
                yield lg, sub, None
            elif sub.name in EXISTING_CATEGORY:
                for f in sorted(sub.rglob("*")):
                    if f.is_dir() and is_fixture(f):
                        yield lg, f, sub.name
            else:
                # An already-nested area we don't know about: recurse.
                for f in sorted(sub.rglob("*")):
                    if f.is_dir() and is_fixture(f):
                        yield lg, f, None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--lang", default=None)
    ap.add_argument("--summary", action="store_true", help="only print bucket counts")
    ap.add_argument("--emit-moves", action="store_true", help="print git mv commands")
    args = ap.parse_args()

    rows = []
    for lg, fdir, existing in discover(args.lang):
        name = fdir.name
        slug = slug_of(name)
        if existing and existing in EXISTING_CATEGORY:
            area, sub = EXISTING_CATEGORY[existing]
        else:
            area, sub = classify(slug)
        num = (re.match(r"^(\d+)-", name) or [None, ""])[1]
        cur = fdir.relative_to(ROOT).as_posix()
        tgt = (FIXTURES / lg / area / sub / name).relative_to(ROOT).as_posix()
        rows.append((lg, num, name, slug, area, sub, cur, tgt))

    counts: dict[tuple[str, str], int] = {}
    for r in rows:
        counts[(r[4], r[5])] = counts.get((r[4], r[5]), 0) + 1

    if args.emit_moves:
        for lg, num, name, slug, area, sub, cur, tgt in rows:
            if cur != tgt:
                print(f"mkdir -p {os.path.dirname(tgt)} && git mv {cur} {tgt}")
        return 0

    if not args.summary:
        print("lang,num,area,sub,slug,current_path,target_path")
        for lg, num, name, slug, area, sub, cur, tgt in sorted(rows, key=lambda r: (r[4], r[5], int(r[1] or 0))):
            print(f"{lg},{num},{area},{sub},{slug},{cur},{tgt}")

    # Summary always to stderr.
    area_tot: dict[str, int] = {}
    for (area, sub), n in counts.items():
        area_tot[area] = area_tot.get(area, 0) + n
    print(f"\n=== {len(rows)} fixtures ===", file=sys.stderr)
    for area in sorted(area_tot, key=lambda a: -area_tot[a]):
        print(f"{area:16} {area_tot[area]:5}", file=sys.stderr)
        for (a, s), n in sorted(counts.items(), key=lambda kv: -kv[1]):
            if a == area:
                print(f"    {s:24} {n:5}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
