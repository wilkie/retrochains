# C++ fixture corpus (BCC 2 only)

A plan to build out a C++ fixture corpus for the one oracle that
actually compiles C++ — **Borland C++ 2.0** (`BCC.EXE`). Microsoft C
5.x (`CL.EXE`) is a C-only compiler and never participates in this
corpus.

This document is written so it can be handed to a fixture-authoring
model (e.g. Sonnet) to **produce fixtures** band-by-band. It specifies
the directory layout, the per-fixture mechanics, the feature taxonomy
in ABI-dependency order, and the scope boundary for BC++ 2.0.

## Status / why now

The corpus to date is **all C** — 4144 `.c` fixtures, zero C++ — yet
the BCC oracle is a real C++ compiler. Everything reimplemented so far
is the *C floor of a C++ compiler*. The C++ object model (name
mangling, vtables, ctor/dtor sequencing, static init, multiple
inheritance) is the bulk of a 1991 compiler's real, undocumented
behavior and is entirely unbuilt and **unmeasured** — there are no
`.cpp` fixtures to measure it with. This plan creates that measurement
surface.

C++ is a **BCC-only** effort. The MSC side is untouched by this work.

## Directory restructure (Phase 0)

The existing flat `fixtures/<name>/` tree is split by language so the
two corpora are reportable independently and the C++ tree (initially
~0% pass) never drags the C baseline:

```
fixtures/
  c/                              # the existing C corpus, moved here verbatim
    001-empty-main/
    002-empty-main-obj/
    ...                           # all 4144 dirs, unchanged contents
  cpp/                            # NEW — the C++ corpus, BCC-only
    0001-empty-main-cpp/
    ...
```

The move is mechanical (`git mv fixtures/<NNNN> fixtures/c/<NNNN>` for
every dir) but touches a few hardcoded paths:

- `crates/fixtures/src/main.rs` — `verify_all` hardcodes
  `workspace_root.join("fixtures")`. Add a `--fixtures-dir <path>`
  flag (default `fixtures/c`) threaded through `verify`, `verify-all`,
  `capture`, and `dashboard`. `Fixture::load` already takes a root, so
  only the discovery walk and the dashboard scan change.
- `CLAUDE.md` — the baseline verify command becomes
  `target/debug/xfix verify-all --toolchain ours --fixtures-dir fixtures/c`.
  The 4072/2 baseline is unchanged by a pure move.
- Any doc referencing `fixtures/<NNNN>` paths.

After Phase 0 the canonical runs are:

```
# existing C corpus (both compilers care)
xfix verify-all --compiler bcc --fixtures-dir fixtures/c
xfix verify-all --compiler msc --fixtures-dir fixtures/c

# new C++ corpus (BCC only)
xfix verify-all --compiler bcc --fixtures-dir fixtures/cpp
```

A pure move must reproduce the C baseline exactly (4072 pass, 2 fail,
same two failing fixtures) before any C++ fixture is added.

## Per-fixture layout and mechanics

A C++ fixture mirrors a C fixture exactly, with two differences: the
source is `.cpp`, and there is **only** an `invocation.bcc.toml`
(never an `invocation.msc.toml` — that is what keeps the tree
BCC-only).

```
fixtures/cpp/0001-empty-main-cpp/
  HELLO.CPP                       # the source
  invocation.bcc.toml            # how the oracle is invoked
  expected/
    bcc/
      manifest.toml               # exit code, stdout/stderr sha, output sizes+sha+mtime
      HELLO.ASM                   # -S tier golden (human-readable intent)
      HELLO.OBJ                   # -c tier golden (the byte-exact target)
```

`invocation.bcc.toml` for the ASM tier:

```toml
description = "Empty main() compiled as C++, -S. Smallest C++ fixture: proves .cpp round-trips through the BCC oracle."
tool = "bcc"
args = ["-S", "-ms", "HELLO.CPP"]
inputs = ["HELLO.CPP"]
```

`BCC.EXE` dispatches to C++ by the `.cpp` extension; `-P` forces C++
for any extension if ever needed. The OBJ tier uses `args = ["-c",
"-ms", "HELLO.CPP"]`. Capture both tiers — the `.ASM` documents
intent, the `.OBJ` is the byte-exact bar.

### Capture workflow

Capture reuses the existing `bcc` toolchain profile (BC2.zip + DOSBox +
`faketime` `bc2`) unchanged:

```
xfix capture fixtures/cpp/0001-empty-main-cpp --compiler bcc
```

Notes for the authoring model:

- Use `timeout 60` per fixture. BCC `-c` is known to hang on some
  shapes (see the C corpus's struct-value-arg note); C++ ctors and
  temporaries may hit analogues. If a shape hangs, reroute it (smaller
  shape) and record it.
- Author the source first, capture, then `xfix verify ... --compiler
  bcc` to confirm the golden round-trips. Never hand-write goldens —
  always capture from the oracle.
- Keep each `HELLO.CPP` to the **smallest shape that isolates one
  feature delta** (the C corpus discipline: `001-empty-main` →
  `003-return-constant` → …). Tiny sources make the OBJ diff equal the
  ABI delta and nothing else.

### Naming

`fixtures/cpp/NNNN-<band>-<shape>/`, four-digit numbers, grouped by
band so the corpus reads in dependency order:

- `0001-empty-main-cpp`
- `0100-mangle-fn-one-int-arg`
- `0300-ctor-default-local`
- `0500-virtual-one-method`

## Scope — what BC++ 2.0 is (and is not)

Borland C++ 2.0 (1991) is **cfront-2.x-era** C++. Author only what it
supports; do **not** author the post-2.0 features (they will not
compile and are out of scope for this oracle).

**In scope:** classes & member layout, access control
(`public`/`private`/`protected`), constructors/destructors, single
**and multiple** inheritance, virtual base classes, virtual
functions/vtables, operator overloading, function overloading +
overload resolution, references, default arguments, inline member
functions, `new`/`delete`, `friend`, static members, `const` member
functions / `this`, nested classes, pointers-to-members.

**Out of scope (post-2.0 — do not author):** templates, exceptions,
RTTI, namespaces, the STL, `bool` as a builtin, anything C++98 and
later.

## The taxonomy — ABI-dependency order

You cannot author vtable fixtures before class layout, and nothing is
verifiable before name mangling (every symbol in every OBJ depends on
it). Author bands roughly top-to-bottom; within a band, smallest
shape first. Target ~10–40 fixtures per band; the initial corpus lands
on the order of **250–500 fixtures**.

### Band 0 — C-as-C++ baseline (~15)

The same trivial C programs compiled as `.cpp`. This surfaces the
**C-vs-C++ codegen deltas** on code we already understand, and is
where mangling, startup, and segment differences first appear.

Author: empty main, return-constant, int local, add two locals, a
call to a free function, a global int. Cheap to produce by copying a
handful of the existing trivial `fixtures/c` sources to `.cpp` and
re-capturing. Watch for: `char`-literal type (`int` in C, `char` in
C++), mandatory prototypes, `const` having internal linkage in C++,
and how/whether `main` itself is mangled (it is not).

### Band 1 — name mangling (~30) — DECODE FIRST

Free functions with varied signatures; the payoff is in the symbol
table, not the instructions. **Every later band depends on this.**

Decode Borland's scheme empirically. The known shape is a
qualified-name + `$q` + argument-type-code string, e.g. roughly
`@f$qi` for `f(int)`. Pin down the codes by authoring one fixture per
parameter type and reading the EXTDEF/PUBDEF names:

- `f(void)`, `f(int)`, `f(char)`, `f(unsigned)`, `f(long)`,
  `f(int,int)`, `f(int*)`, `f(int&)`, `f(const int*)`, `f(char*)`,
  `f(double)`, returning each type, etc.

Do not assume the codes — let the fixtures define the mangling table.
Record the decoded table in `specs/bcc/` once stable.

### Band 2 — function overloading (~15)

`f(int)` vs `f(char*)` vs `f(double)` with a call site — verifies
overload *resolution* picks the right mangled symbol.

### Band 3 — references (~15)

`int &r = x;`, reference parameters, reference returns. Codegen is
pointer codegen; the front end and mangling differ.

### Band 4 — default arguments (~10)

`int f(int a, int b = 5);` — caller-side argument insertion.

### Band 5 — classes: layout & access (~30)

POD-ish classes: member offsets (should match the equivalent C
struct), access control (compile-time only — no codegen change), the
implicit `this` pointer in non-static member functions, and
member-function name mangling (`Class::method`, roughly
`@Class@method$q...`). One method, multiple methods, member read/write
through `this`.

### Band 6 — constructors / destructors (~40) — BIG ABI BAND

The first deep object-model band. Cover, smallest-first:

- default ctor, parameterized ctor, copy ctor, dtor;
- ctor/dtor **sequencing**: a local object's dtor at scope exit;
  member-subobject ctor/dtor ordering; temporary lifetime;
- **static / global object init**: Borland emits a static-constructor
  segment and registration records to run ctors before `main` and
  dtors at exit — these must match byte-for-byte. This is the hardest
  part of the band; isolate it (a single global with a ctor).

### Band 7 — `new` / `delete` (~15)

`new T`, `delete p`, `new T[n]`, `delete[] p`. Verify the
`operator new`/`operator delete` calls (Borland's helper names) and
the hidden element-count word on array forms.

### Band 8 — single inheritance (~20)

Base subobject layout, derived ctor chaining to the base ctor,
inherited member functions, member access through a base subobject.
`this` adjustment is trivial here (offset 0).

### Band 9 — virtual functions (~30)

The vtable band. vtable layout and contents, vptr placement within the
object, virtual call dispatch, the vtable emitted as a named (mangled)
data object, pure-virtual functions. One virtual method, an override,
a call through a base pointer.

### Band 10 — multiple inheritance (~25)

Multiple base subobjects, a vptr per polymorphic base, **`this`-
adjustment thunks**, and derived→base pointer conversions (the
non-zero offset case).

### Band 11 — virtual base classes (~15)

The hard layout case: virtual-base pointers/offsets, the diamond,
shared-subobject construction order.

### Band 12 — operator overloading (~20)

Mangled operator names; `operator=`, `operator+`, `operator[]`,
`operator<<`, member vs free-function operators, conversion operators.

### Band 13 — tail (~20)

Static data members (definition + linkage), `friend` functions/classes,
`const` member functions, pointers-to-members, nested classes.

## Workflow note — capture and tiers

Capture is the existing single-tool `bcc` path (`-S` and `-c`), so no
pipeline extension is needed for the compile tiers. A link/run tier
(`tlink` + execute) would additionally require the C++ runtime startup
and the static-init/teardown machinery to actually run — defer that;
the `-S`/`-c` OBJ comparison already exposes the full mangling,
vtable, and static-ctor-segment ABI, which is the reverse-engineering
goal. If a run tier is wanted later, it shares the open question in
`OPEN_QUESTIONS.md` about multi-tool pipeline capture.

## Ordering

1. **Phase 0** — restructure (`fixtures/c`, `fixtures/cpp`), add
   `--fixtures-dir`, reproduce the C baseline exactly, and author +
   capture a single `0001-empty-main-cpp` to prove the `.cpp` capture
   path end-to-end.
2. **Band 0 + Band 1** — baseline and mangling. High decode value;
   unblocks everything. Land the mangling table before anything else.
3. **Bands 2–5** — overloading, references, default args, class
   layout. Pure-ish front-end + simple codegen.
4. **Band 6 (ctor/dtor + static init)** — the first deep ABI band;
   the spine of the object model. Do not start virtuals until ctor/
   dtor and object layout are solid.
5. **Bands 7–13** — new/delete, then the inheritance/vtable ladder
   (single → virtual fns → multiple → virtual bases), then operators
   and the tail.

Author and capture a band fully before moving on; the implementation
work (a C++ front end + object-model codegen in the `bcc` crate, gated
on `.cpp`/`-P`) then follows the same corpus-driven loop the C work
used, band by band.

## Definition of done (for the corpus)

The C++ corpus is "scoped out and bootstrapped" when:

- `fixtures/c` and `fixtures/cpp` exist; the C baseline reproduces
  exactly under `--fixtures-dir fixtures/c`.
- `--fixtures-dir` is wired through verify / verify-all / capture /
  dashboard, and the dashboard reports the two corpora separately.
- Band 0 and Band 1 are authored and captured, and the decoded Borland
  mangling table is recorded in `specs/bcc/`.

The corpus is "complete enough to drive a C++ reimplementation" when
every band above has its target fixture count captured (~250–500
total), spanning the in-scope BC++ 2.0 feature set, each fixture the
smallest shape isolating one ABI delta.

## Out of scope for this document

The actual C++ reimplementation (parser for class/overload
declarations, type + overload-resolution layer, object-model codegen,
mangling emitter, vtable + static-ctor-segment emission in the `bcc`
crate) is a consequence of this corpus, not part of it. The corpus is
the spec; author it first, in dependency order.
