# Phase 2 — fixture corpus expansion

## Status at end of Phase 1

3697 / 3697 fixtures byte-exact under `xfix verify` for every recorded
invocation flag combination. The full corpus regression check passes
without diffs after the final commit at the close of Phase 1.

Phase 1 closed with substantial new compiler machinery landed:

- Far / huge pointer types (`Type::FarPointer { pointee, is_huge }`),
  parser support for `far` / `huge` / `near` keywords, and the post-parse
  promotion pass that rewrites pointers under compact / large / huge
  models.
- Huge memory model whole-program layout — `<MODULE>_TEXT` /
  `<MODULE>_DATA` segments, `FAR_DATA` class, per-function DS reload,
  and the post-pass that rewrites the asm output.
- Function-pointer 4-byte slot promotion under far-code models, with
  the matching `call far ptr [bp+disp]` indirect-call encoding.
- Inline `asm { ... }` block and statement forms, including C variable
  substitution and the `_AX` / `_BX` / `_CX` / `_DX` pseudo-register
  family for return-value passthrough.
- Long-scrutinee switch lowering through a compare-both-halves loop
  (`SwitchStrategy::LongLinearSearch`).
- Three new long-arithmetic peepholes (`((long)hi << 16) | lo`,
  `(long)i * l + K`, huge-pointer subtraction via `N_PSBP@ + N_LDIV@`).
- `arr[var].field` for stack struct arrays with non-power-of-2 stride,
  on both the read (push/pop-fenced chain) and write sides.
- The `[BP+SI+disp]` addressing-mode peephole for stack-char-array
  subscript access with the index in SI.

The byte-exact match gives **strong confidence in the patterns the
fixtures explicitly exercise**. It gives **much weaker confidence
across the surface area each piece of machinery plausibly covers**.

## Coverage at a glance

| Dimension | Distribution |
|---|---|
| Memory model `-ms` | 3663 |
| `-ml` | 17 |
| `-mm` | 10 |
| `-mc` | 4 |
| `-mh` | 2 |
| `-mt` | 1 |
| `-O` (optimize) | 4 |
| `-1` / `-2` (80186 / 80286 target) | 4 each |
| `-N` (stack check) | 2 |
| `-K` (default `char` unsigned) | 2 |
| Fixture line count, p50 | 6 |
| Fixture line count, p95 | 11 |
| Fixtures > 30 lines | ~1 |
| Inline asm | 5 |
| `printf` callsites | 13 |
| `scanf` / `malloc` / `FILE *` | 0 |

This shape — a vast tail of small-model snippets with very thin
coverage on every other axis — explains why the byte-exact pass rate
is misleading as a confidence signal. A regression that only fires
under `-mh` with a global initializer can pass 3696 fixtures and break
production code, and most of the machinery we just landed has only
1–10 tests guarding it.

## Phase 2 goals

The goal of Phase 2 is **breadth before depth**. We want the corpus
to be a stronger contract — the surface area we plausibly cover
should match the surface area we actually exercise. Concretely:

- Bring each non-small memory model up to **at least 50 fixtures**
  spanning the same complexity bands as the small-model corpus.
- Add coverage for the **DOS / runtime intrinsic surface** that real
  BC2 code uses (`int86`, `_dos_*`, `MK_FP`, `peek`/`poke`,
  `geninterrupt`).
- Complete the **far-pointer arithmetic and storage-shape matrix** —
  comparisons, mixed near/far operands, struct fields, arrays.
- Fill the **switch matrix** — char and unsigned-long scrutinees,
  larger case counts, default-in-middle, fall-through.
- Add **medium-size translation units** (50–500 lines) that stress
  cross-function bookkeeping the median 6-line fixture doesn't reach.
- Tighten edge-case flag combinations — `-O` interactions, `-1` /
  `-2` with calls and shifts, `-K` with comparisons.

## Priority 1 — non-small memory models

**Why it's first**: every piece of model-specific codegen we landed in
Phase 1 (far pointer init / deref / postinc, huge model layout
rewrite, fn-ptr promotion, far-arg passing, indirect far call) rests
on a tiny handful of fixtures. Any non-trivial real medium / large /
huge program is outside our tested envelope. Bugs in the rewrite
post-pass or the FIXUP-frame computation can hide indefinitely.

**Target**: at least 50 fixtures per model, replicating the same
complexity bands the small-model corpus already covers. ~250 fixtures
total. Each band should add roughly:

- **Globals**: initialized int / long / char / struct / array (`-mc`,
  `-ml`, `-mh` particularly — the DGROUP-vs-segment-frame FIXUP
  difference is invisible in the existing fixtures).
- **Function with locals**: int / long / char locals, register promotion,
  callee-saved register handling in the model's frame shape.
- **Pointer ops**: deref read / write / postinc / postdec / arithmetic
  for every pointee size (char, int, long, struct).
- **Function calls**: TU-local and extern, varying arg counts and
  types. Each model's frame offset (`[bp+4]` vs `[bp+6]`) needs to be
  visible.
- **String literals**: pooling, FIXUP frame.
- **Switch**: at least one Chained, one LinearSearch, one JumpTable per
  model. The current corpus has all three under `-ms` but only one
  each (or zero) under far-code models.

**Specific concerns**:

- Huge model (`-mh`) currently has 2 fixtures. The `rewrite_for_huge_model`
  text post-pass strips `DGROUP:` prefixes, merges `_DATA` / `_BSS` into
  `<MODULE>_DATA`, drops `d@` / `d@w` / `b@` / `b@w` labels, and
  inserts the per-function DS reload. Every one of those rewrites is
  guarded by 1–2 tests. A medium-complexity huge-model program
  (initialized + uninitialized globals + multiple functions + an
  extern call) almost certainly exposes shapes we haven't seen.
- Compact model (`-mc`) has 4 fixtures. We promote data pointers to
  far but data lives in DGROUP — the FIXUP behavior is closer to small
  than to huge. The promotion-with-NearPointer-explicit-override path
  (fixture 1748) is the only test covering the override.
- Medium model (`-mm`) has 10 fixtures. Code is far but data is near.
  Fn-pointer promotion to 4-byte slots is the medium-only path; only
  fixture 2211 exercises it.

## Priority 2 — DOS / runtime intrinsics

**Why**: zero coverage today. Real BC2 programs use these constantly.
Whatever near / far frame the intrinsic call patterns produce isn't
being tested against the oracle.

Suggested initial coverage (~15 fixtures):

- `int86()` with both `union REGS` and `__dpmi_regs` shapes — exercises
  struct-by-value arg passing through `N_SPUSH@` and the `int86` call.
- `intdos()` — similar shape but the DOS-services-specific variant.
- `geninterrupt(N)` — inline interrupt; BCC may emit `int N` directly
  with no helper call.
- `_dos_open` / `_dos_read` / `_dos_close` family — typical file I/O
  shape with far pointer args and int return.
- `MK_FP(seg, off)` and the `FP_SEG` / `FP_OFF` macros — far pointer
  literal construction. These appear in `<dos.h>` as macros that
  expand to bit twiddling; codegen needs to handle the `(void __far *)
  (((unsigned long) seg << 16) | off)` shape.
- `peek(seg, off)` / `peekb(seg, off)` / `poke` / `pokeb` — direct
  segmented memory access. These almost certainly compile to a 4-byte
  far-pointer construction + dereference.
- `segread()` — fills a `SREGS` struct with current segment registers.
- `getvect()` / `setvect()` — interrupt vector access.

## Priority 3 — far / huge pointer matrix

**Why**: the existing fixtures (1649-1652, 1771-1774, 2058, 2250) cover
deref read, write, postinc, postdec, ==, subtraction. They do not
cover:

- **Comparison operators other than `==`**: `<`, `>`, `<=`, `>=`,
  `!=`. The `N_PCMP@` helper sets the standard flags so these *should*
  fall out, but no test verifies it. Negative-pointer-arithmetic
  branches around `0` are easy to break.
- **Mixed near / far compare**: Borland allows `near_ptr == far_ptr` with
  an implicit conversion. The conversion path is plausibly broken.
- **`far + int`**: pointer arithmetic with an int operand (the
  non-trivial case where stride scaling happens). `N_PADA@` was tested
  with constant stride; variable-int-offset isn't.
- **Far pointer as a struct field**: `struct S { int far *p; };`. The
  4-byte slot inside a struct affects struct layout and field-access
  codegen.
- **Array of far pointers**: `int far *arr[10];`. Stride-4 indexing
  with the deref through `les bx`.
- **Far pointer return value**: `int far *get_buf(void);`. ABI is
  likely DX:AX, but no test verifies it.
- **Huge pointer arithmetic with non-stride-2 element types**: 1771
  tests `int huge *` (stride 2); a `struct huge *` with stride 6 or 12
  would exercise the helper differently.

Target: ~20 fixtures filling this matrix.

## Priority 4 — switch matrix completion

**Why**: 94 switch fixtures sounds substantial but the matrix has
gaps:

- **Switch on `char`**: 0 fixtures. The scrutinee load differs
  (byte-form + widen). Our codegen for switch char hasn't been
  validated against the oracle.
- **Switch on `unsigned long`**: 0. Signed long is fixture 1913. The
  unsigned variant should still use `LongLinearSearch` but the
  long-compare helper might be unsigned (`N_LUCMP@`?). Untested.
- **Switch with > 32 cases**: BCC may choose a different dispatch
  shape past some threshold. Current `pick_switch_strategy` picks
  jump-table for contiguous ranges; we don't know what BCC does for,
  say, 64 cases scattered across a range of 200.
- **Default in the middle**: `case 1: ... case 2: ... default: ... case
  3: ...`. The dispatch order may need to keep the default in a
  specific position in the value table.
- **Fall-through chains**: `case 1: case 2: case 3: <body>; break;`.
  Our codegen handles this via no `break;` at the end of each case,
  but the OBJ-level interaction with the linear-search table layout
  isn't exercised at non-trivial fan-in.
- **Switch with side effects in the scrutinee**: `switch (f())`,
  `switch (a[i]++)`. The scrutinee gets evaluated once before
  spilling; the spill-and-walk pattern interaction with side-effect
  expressions isn't covered.

Target: ~15 fixtures.

## Priority 5 — medium-size translation units

**Why**: fixture line count distribution is brutally skewed.

```
p25: 5 lines      median: 6      p75: 7      p95: 11      max: 22683
```

The median fixture has one or two statements after `main`. Real BC2
TUs (game source, utility programs, library implementations) are
hundreds of lines. Several compiler-state behaviors only show up at
that scale:

- **Label numbering across many control-flow nodes**: our slot
  planner reserves slot ranges per construct. Long functions with
  many ifs / loops / switches stress the numbering and may reveal
  off-by-N bugs that pass on small examples.
- **Frame layout interactions across many locals**: the mixed-frame
  layout rule (`<16-byte top bucket and ≥16-byte bottom bucket`)
  is documented in MEMORY.md as load-bearing but is exercised by a
  small number of fixtures.
- **Publics ordering**: BCC orders publics by a complex rule
  (currently 80.8% correct per the existing memory note). More
  fixtures with diverse global + extern + helper mixes would let us
  pin down the remaining rule fragments.
- **Long compilation units expose label-numbering edge cases**:
  the slot counter is a `u32` but the encoded label numbers in
  `@<func>@<num>` interact with the line-mapping comments. Long
  functions with many comments-with-asm could expose interactions
  the small fixtures don't.
- **Many cross-function patterns stress publics / extdef ordering**:
  the publics list emission is sensitive to name length and
  alphabetical bucketing. A TU with ~30 functions of various name
  lengths and intermixed extern calls would stress this.

Target: ~20 fixtures in the 50–500 line range. Sources of inspiration:

- Small benchmark programs from the era (e.g. dhrystone, sieve,
  prime-counters at non-trivial size).
- BC2's own utility examples shipped on the install disks.
- Reimplementations of `<string.h>` functions as a TU (strcpy,
  strcmp, strlen, strcat together).
- Simple state machines with many enum cases dispatched via switch.

## Priority 6 — libc / runtime helper usage

**Why**: today's coverage:

```
printf:      13      strcpy: 1      scanf:  0      malloc: 0
puts:         8      strlen: 1      memcpy: 0      free:   0
putchar:      1      strcmp: 0      memset: 0      FILE *: 0
```

We test calls to externs (which exercises FIXUP / EXTDEF emission) but
nothing about the patterns programs actually use.

Suggested additions:

- `scanf("%d", &x)` — exercises both extern call and address-of-int
  arg passing. Variants with `%s`, `%ld`, multiple args.
- `strcpy` / `strcmp` / `strlen` / `strcat` chains — common idioms
  that ought to compile to byte-identical sequences against BC2.
- `printf` with non-trivial format strings — `%d` / `%ld` / `%s` /
  `%x` / `%c` mixed; long args; pointer args. The `printf` arg-push
  shape varies subtly with arg types.
- `memcpy` / `memset` / `memcmp` — bulk byte ops; verify near vs far
  pointer arg shape under each model.
- `malloc` / `free` / `realloc` — `void *` return value handling.
- Simple FILE I/O: `fopen` / `fclose` / `fread` / `fwrite`. The FILE *
  is a pointer-to-struct; the codegen interaction with extern struct
  types is essentially untested.
- `atoi` / `atol` / `itoa` / `ltoa` — conversion routines that
  appear in real programs.

Target: ~25 fixtures.

## Priority 7 — flag-combo edge cases

**Why**: most flags are tested only in isolation. Combinations
plausibly hit codegen we haven't validated.

Specific combinations worth adding:

- **`-O`** with various idioms — does optimize change the
  `add ax, 1` vs `inc ax` selection in unexpected places? Does it
  affect frame layout? Currently 4 fixtures.
- **`-1` / `-2`** (80186 / 286 target) with calls — `enter` / `leave`
  collapse and call-site frame setup interact. Current 4 fixtures each
  are likely minimal.
- **`-1` / `-2` with shifts ≥ 4** — the multibit `shl reg, imm` (`C1 /4
  ib`) is the 186+ encoding; thresholds and the existing peephole
  `fold_shl_to_multibit` may have edge cases.
- **`-K`** (default `char` unsigned) with mixed signed/unsigned
  comparisons — does the comparison-mnemonic selection rule fire
  correctly when `char` becomes the unsigned scrutinee?
- **`-N`** (stack check) with various stack sizes — the `___brklvl`
  check is currently exercised by 2 fixtures. Functions with > 256
  byte frames, with calls, with structs may stress the check
  placement.
- **`-G`** (codegen flags) — 1 fixture. Unknown what other shapes
  this affects.

Target: ~15 fixtures.

## Priority 8 — inline asm shape expansion

**Why**: 5 fixtures total — the simplest shapes only.

Untested asm patterns:

- **asm with branch targets**: `asm cmp ax, 5 / asm jl skip / ... /
  skip:`. The label-emission and forward-reference handling inside
  asm bodies.
- **asm jumps to C labels**: `asm jmp <c_label>;` where `<c_label>`
  is a `goto` label in the surrounding function.
- **asm using `_BX` / `_CX` / `_DX`** pseudo-registers in *non*-return
  positions. We special-cased `_AX` in `return _AX;`; the others may
  appear in other contexts.
- **asm in non-`main` functions** — fixture 2122 is the only multi-
  function asm test, and the asm body is trivial.
- **asm modifying C variables**: `asm mov word ptr <c_var>, ax;`
  where `<c_var>` is a stack local. We substitute the variable name
  but only in source-operand positions; destination-position
  substitution may need fixes.
- **asm with `byte ptr` operands referencing C `char` locals**: only
  the word-ptr substitution path is tested today.

Target: ~10 fixtures.

## Priority 9 — float / double matrix

**Why**: 60 float/double fixtures total but the FPU codegen surface
is large.

- **FPU stack-depth interleaves**: 8087 has 8 stack slots. Nested
  expressions like `(a + b) * (c + d) / (e + f)` push multiple
  intermediate results. Stack overflow / underflow handling at the
  codegen level isn't exercised at depth.
- **`printf("%f", x)`**: float arg gets promoted to double per default
  argument promotion. Our `arg_ty` defaulting handles this but the
  exact byte sequence under varying float / double mixes isn't
  validated.
- **Float compare ordered / unordered (NaN)**: Borland's specific
  behavior for `nan < 0` etc. The 8087 `fcom` family sets condition
  codes that get translated through `fstsw / sahf`; the unsigned-Jcc
  mnemonic selection has its own rule.
- **`long double` (80-bit)**: the native FPU width. Likely not
  exercised at all.
- **FPU + integer interleave**: a function that does some FPU work,
  then some integer work, then more FPU work. Whether we reload the
  FPU state correctly across CPU op interleaves is untested.

Target: ~15 fixtures.

## Priority 10 — preprocessor and struct edge cases

Lower priority because both subsystems pass their existing fixtures
cleanly and the gaps are more about *breadth of valid programs* than
about known bugs.

Preprocessor candidates (~10 fixtures):

- Token pasting (`a##b`) and stringification (`#x`).
- Nested macro expansion at depth ≥ 3.
- `defined()` in `#if` expressions.
- Macros that take type names as arguments.
- Multi-line macros with continuations.

Struct candidates (~10 fixtures):

- Nested struct > 2 levels.
- Struct with mixed-alignment fields stressing the padding rule.
- Anonymous union inside struct (Borland extension).
- Struct as function arg by-value at sizes 5, 7, 9 (odd sizes between
  the small-via-DX:AX and large-via-N_SCOPY@ thresholds).
- Self-referencing struct pointer at multiple depths
  (`struct N { struct N *prev, *next; };`).

## Workflow note — capture vs verify

Adding new fixtures means running `xfix capture` against the BC2
oracle in DOSBox with `faketime` set. The current pipeline mostly
covers `bcc` invocations; the `tlink` step (if we want to test linked
outputs) is its own surface. For Phase 2's libc / printf / dos-intrinsic
fixtures, we'll likely need to extend the capture pipeline to handle
multi-tool invocations (`bcc -c` then `tlink` then run-and-capture-
output), since the `-c` step alone won't exercise the full
extern-resolution path.

This is recorded as an **open question** in OPEN_QUESTIONS.md for
Phase 2 planning: how do we want to capture and verify multi-tool
pipelines? Options include:

1. Extend `invocation.toml` to support a list of `[[step]]` tables
   each describing one tool invocation; `xfix capture` and `xfix
   verify` walk the list and validate each step's outputs.
2. Keep `invocation.toml` single-tool but add a parallel
   `pipeline.toml` for multi-step fixtures.
3. Capture the full DOSBox session output (terminal log + every
   produced file) and verify against that, treating the pipeline as a
   single oracle "run."

Option 1 is the cleanest extension of the existing schema and is the
recommended path.

## Ordering

A reasonable interleave that keeps each commit's blast radius small:

1. Start with **Priority 1** (non-small models). Easy to add fixtures
   one at a time, each one validates a specific path we already wrote
   code for. Most likely to surface latent bugs in the recently-landed
   far-pointer / huge-model machinery.
2. Once non-small models are at ~30 fixtures each, move to **Priority
   3** (far-pointer matrix). These reuse the model infrastructure
   from Priority 1 and probe specific operators we haven't tested.
3. **Priority 6** (libc usage) in parallel with Priority 3 — both
   exercise extern emission and FIXUP frames.
4. **Priority 4** (switch matrix) — self-contained, completes the
   switch dispatch surface.
5. **Priority 2** (DOS intrinsics) — depends on having a few medium-
   model fixtures already (`int86` and friends often appear in `-mm`
   programs).
6. **Priority 8** (asm shapes) — small effort, completes the
   inline-asm surface.
7. **Priority 5** (medium-size TUs) and **Priority 7** (flag combos)
   as ongoing work — these are mostly stress tests rather than
   targeted gap-filling, so they can run continuously.
8. **Priority 9** (float matrix) and **Priority 10** (preprocessor /
   struct edges) when the earlier priorities are landing reliably.

## Definition of done

Phase 2 closes when:

- Every memory model has ≥ 50 fixtures spanning the same complexity
  bands as the small-model corpus.
- The far-pointer matrix has comparison operators, mixed
  near / far operands, struct-field and array storage, and
  return-value handling explicitly tested.
- DOS / runtime intrinsics have ≥ 15 fixtures covering the common
  shapes.
- Switch matrix covers char, unsigned long, > 32-case, default-in-
  middle, and fall-through chains.
- The corpus has ≥ 20 medium-size (50–500 line) fixtures.
- Libc usage has ≥ 25 fixtures spanning the canonical C library
  routines (`<stdio.h>`, `<string.h>`, `<stdlib.h>`).
- Every flag combination documented in `RUNNING_BCC.md` has at least
  one fixture exercising it in combination with at least one other
  flag.
- Inline asm has ≥ 10 fixtures covering labels, jumps, all GP
  pseudo-registers, and non-`main` host functions.

After Phase 2, the **byte-exact pass rate continues to mean "we
reproduce BCC's output across a representative cross-section of real
BC2 programs,"** not just "we reproduce BCC's output across a
collection of 6-line snippets that happen to stress small-model `-ms`
codegen."
