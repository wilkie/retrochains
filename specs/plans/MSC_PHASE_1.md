# Phase 1 — Microsoft C 5.0 bring-up

The plan for getting `crates/msc/` from zero to "first byte-exact MSC
fixture corpus" in roughly the same shape that `crates/bcc/` reached
at the end of its own Phase 1. This is the second compiler target in
the project, following the layout migration described in
[`SECOND_COMPILER.md`](SECOND_COMPILER.md).

## Status at start of Phase 1

- Oracle profile `microsoft-msc5` lands in `crates/oracle/` (see
  `OracleConfig::for_msc500_workspace`). Lazy-extracts `MSC500.zip`
  to `.msc500/` and drives `CL.EXE` under DOSBox the same way the
  BC2 oracle drives `BCC.EXE`.
- Single MSC fixture: `4075-msc-empty-main-obj` — `int main(void)
  { return 0; }` compiled with `cl /c /AS`. Captured byte-exact;
  `xfix verify --toolchain oracle --compiler msc fixtures/4075-…`
  reproduces it deterministically.
- `crates/msc/` does **not** exist. `xfix verify-all --toolchain
  ours --compiler msc` reports "tool not yet implemented in our
  toolchain: cl" — the regression anchor for everything we land
  next.
- BC2 corpus regression: 4074 / 4074 byte-exact, unchanged by the
  oracle generalization.

## What the empty-main fixture revealed

The smallest possible MSC OBJ already exposes ~6 structural
differences from BCC's OBJ for the same source. Recording them up
front because every one will need a deliberate decision in the
reimplementation.

| Aspect              | BCC 2.0 emits                       | MSC 5.0 emits                                       |
|---------------------|-------------------------------------|-----------------------------------------------------|
| THEADR name         | `hello.c` (lowercased)              | `HELLO.C` (uppercased)                              |
| Vendor COMENT       | `TC86 Borland Turbo C++ 2.0`        | `MS C`                                              |
| Debug-format COMENT | none in `-c` empty-main             | `CV7` (CodeView 7 hint), `0sO`, …                   |
| Default-library COMENT | `Cl81` (class `A1`)              | `SLIBCE` (class `9F`)                               |
| Segments            | 4: `_TEXT/CODE`, `_DATA/DATA`, `_BSS/BSS` + DGROUP | 5: adds `CONST/CONST`                |
| Auto-emitted EXTDEFs | 0 (BCC inlines its startup glue)  | 2: `__acrtused`, `__chkstk`                         |
| Code shape          | `push bp; mov bp,sp; xor ax,ax; jmp short L; L: pop bp; ret` | `push bp; mov bp,sp; call __chkstk; xor ax,ax; pop bp; ret` |
| MODEND tag          | `0x8A`                              | `0xCA` (MODEND with start-address)                  |

Most of these are mechanical (different string constants in COMENT
records, different segment-name table, different sentinel byte).
Two are semantic and load-bearing:

1. **`__chkstk` is always called**, even when the function uses 0
   bytes of stack. CL emits the call unconditionally; LINK eventually
   resolves it from the runtime library. This is the equivalent of
   BCC's `__brklvl` symbol but called per-function rather than
   sentinel-only.
2. **`__acrtused`** is the C runtime "I have a main, please link
   the startup glue" sentinel. EXTDEF + FIXUP referring to it forces
   LINK to pull in the C runtime startup. BCC has no equivalent —
   its runtime startup is unconditional.

## Phase 1 scope

Same shape as BCC's Phase 1: get to **byte-exact OBJ for a defined
small-model bring-up corpus**, with the surface area chosen so each
fixture nails down one specific MSC pattern. Defer everything else
to Phase 2.

**In scope:**

- Small memory model only (`/AS`). No medium / compact / large /
  huge until Phase 2.
- `cl /c <flags> HELLO.C` — compile-only path. Linker (`LINK.EXE`)
  and the MASM-assembled side stay out of the corpus for now;
  fixtures use `cl` as the single oracle tool just like BCC's
  `-c` corpus uses `bcc`.
- No FP code. The MSC 5.0 floating-point story (math emulator vs.
  inline 8087 vs. 8087-required, controlled by `/FPa`/`/FPe`/`/FPi87`
  flags and the `SLIBCA` / `SLIBCE` / `SLIBC7` library choice) is
  its own substantial area — defer to Phase 2.
- Default flags only: `cl /c /AS`. No `/O*`, no `/Z*`, no
  `/G*`. Get the unoptimized baseline correct before adding
  optimizer-on / debug-info-on variants.

**Out of scope, explicitly:**

- `crates/masm/`. MSC's assembler is MASM 5.x — same OMF target as
  TASM but with meaningfully different syntax. We won't need it
  until we have fixtures whose `tool` is `masm`, which is a Phase 2
  decision.
- `crates/link/`. MSC's `LINK.EXE` is a substantially different
  linker from TLINK. The Phase 1 corpus stops at OBJ emission, so
  we don't need it.
- Phase 2 features that BCC has but the Phase 1 MSC corpus won't
  reach: huge model, pseudo-registers (MSC's `_AX`-equivalent is
  `__asm` with intrinsic register names, different syntax), inline
  asm, segment-qualified pointers, `_seg` pointers.

## `crates/msc/` scaffolding — what to land first

The natural slicing roughly mirrors BCC's Phase 1 sequence. Each
slice adds one capability and a few fixtures that wouldn't have
worked before it.

### Slice 1 — empty-main OBJ shell

Land `crates/msc/` with enough scaffolding to emit the bytes
fixture 4075 captured: THEADR (`HELLO.C`), the four vendor
COMENTs, segment definitions for `_TEXT` / `_DATA` / `CONST` / `_BSS`
(grouped into DGROUP), `__acrtused` and `__chkstk` EXTDEFs, the
`_main` PUBDEF, the `__chkstk`-calling LEDATA, the FIXUPs against
both EXTDEFs, MODEND with start-address pointing at `_main`.

No parser at all yet — hardcode the IR for `int main(void) { return
0; }` and let the OMF emitter do its job. Same trick BCC's Slice 1
used (look at `crates/bcc/src/lib.rs` Phase-1 git history). Closes
fixture 4075 alone, but the OMF-emitter scaffolding it leaves
behind unlocks everything in Slice 2.

The bring-up surface for OMF differences here:

- `crates/obj/` may need a sixth segment name slot (BCC's emitter
  hardcodes the 4-segment + DGROUP scheme). Either generalize the
  segment table or add an MSC-specific helper.
- COMENT class codes used by MSC 5.0 that BCC doesn't emit
  (`9F` = default-library, `9D` = end-of-pass-1 marker, `A1` and
  `A3` with different payloads). The OMF reference in
  `specs/formats/OMF.md` covers them at the record-format level;
  Phase 1 fills in the MSC-specific payloads.
- The MODEND `0xCA` variant carries a start-address FIXUP target;
  the existing `0x8A` emitter path doesn't handle it.

### Slice 2 — `return <integer>;`

Trivial codegen extension: the literal in `return K;` lands as
`mov ax, K` instead of `xor ax,ax`. Probably no new IR — just
extend Slice 1's hardcoded shape with an int literal parameter.

Target fixtures (~4):

- `return 0;`, `return 1;`, `return 42;`, `return -1;` —
  small literals.

This is where the int-immediate encoding starts to matter. BCC
emits `mov ax, K` with the standard `B8` opcode + LE bytes; we
should confirm MSC does the same (no obvious reason it wouldn't,
but we said it about the THEADR case too).

### Slice 3 — int locals, return-the-local

Parser comes online: declarations, simple expressions, return
statement. Codegen produces a stack frame with the BP-relative
local layout MSC uses.

This is where MSC's stack frame is going to diverge from BCC's
in interesting ways. Things to characterize per fixture:

- Does MSC use `enter` / `leave` (286-era encoding) or `push bp;
  mov bp,sp; sub sp,N` (8086-era)? `/AS` defaults to 8086 target,
  so probably the latter, but `/G1` /  `/G2` change it.
- Local frame offset: BCC's first local is at `[bp-2]`, then
  `[bp-4]`, etc. MSC may differ (some compilers reserve space at
  `[bp-2]` for a frame pointer chain or `__chkstk` accounting).
- Register pool: BCC uses `{SI, DI, DX, BX, CX}` for int locals
  with the AX-is-working-register convention. MSC's pool is
  likely the same `{SI, DI, BX, DX, CX}` set (every 16-bit DOS C
  compiler converges here) but the *order* and *enregistration
  thresholds* almost certainly differ — fixture pairs that
  declare 1, 2, 3 locals will pin this down.

Target fixtures (~6): single int local, two int locals, three int
locals, one local with `int x = 5;`, one with `int x; x = 5;`,
return-the-local.

### Slice 4 — int arithmetic

Add `+`, `-`, `*`, `/`, `%` over int locals and immediates. By the
end of this slice we should have ~15 fixtures covering the
arithmetic forms BCC's early Phase 1 covered.

Key behavior to characterize: does MSC fold constant subexpressions
the same way BCC does (`return 1 + 2;` → `mov ax, 3`)? Does
`x * 2` lower to `shl ax, 1` (BCC does)? `x / 2` to `sar` (BCC
does, modulo `cdq` for signed). The folder pass and the strength
reduction pass are independent from the parser; characterize them
with fixtures before committing to whether MSC's IR pipeline
mirrors BCC's.

### Slice 5 — control flow

`if` / `else`, `while`, `for`, `do-while`. ~15 fixtures.

Characterization needed: MSC's label numbering scheme, jump
selection (short vs near), and whether MSC emits the same
constant-condition asymmetry we saw with BCC (`if (1)` elides,
`if (0)` keeps the dead branch). Very plausible they differ.

### Slice 6 — function calls

TU-local non-static calls, TU-local statics, externs via
`#include <stdio.h>` (printf for output). MSC's calling
convention is cdecl by default; BCC is too. The push order and
caller-cleanup shape should match, but the wrapper sequences may
differ.

Target fixtures (~15) bring this slice to the same total fixture
count BCC had at end of its own Phase 1 Slice 6 (~50 cumulative).

### Slice 7 — globals

Initialized globals land in `_DATA`. Uninitialized globals land
in `_BSS`. MSC may handle the `CONST` segment differently from
BCC (which doesn't emit a `CONST` segment at all in
`-c` mode). A fixture with a `const int g = 5;` will distinguish
whether MSC routes that to `_DATA` or `CONST`.

The DGROUP-vs-segment FIXUP frame distinction (which kept us busy
in BCC's Phase 1 — `frame: GRPDEF` vs `frame: SEGDEF`) is going
to recur here. Best characterized by paired fixtures that read
the same global through different access paths.

Target fixtures (~10).

### Slice 8 — strings / pointers / arrays

`char *p = "lit";`, `int a[N]`, `*p`, `a[i]`. The string-pool
mechanics will differ — BCC interns into `s@` in `_DATA`; MSC
likely uses `CONST` for string literals.

Target fixtures (~15).

## Bring-up fixture corpus — target

Roughly **70–100 MSC fixtures** by end of Phase 1, mirroring the
BCC Phase 1 closing count after we strip the model-variant
multiplications. Each fixture should be the smallest C source that
exercises one specific MSC pattern.

Fixture naming follows the same scheme: a sequential number plus a
slug. Reserve the **4xxx** range for MSC bring-up so they don't
collide with the existing BCC corpus. Numbering starts at 4075
(`empty-main`) and grows from there.

Each fixture file shape (post-migration):

```
fixtures/4076-msc-return-int-obj/
  HELLO.C
  invocation.msc.toml      # tool="cl", args=["/c","/AS","HELLO.C"]
  expected/msc/
    HELLO.OBJ
    manifest.toml
    stdout
    stderr
```

Same fixture root can in principle add `invocation.bcc.toml` later
to test the equivalent program under both compilers — but the
expectation for Phase 1 is that the MSC fixtures stand alone.
Cross-compiler fixtures (same source, both compilers, divergent
goldens) are a Phase 2 idea.

## Shared infrastructure — what may need to grow

These crates likely need extensions during MSC Phase 1, but the
shape isn't knowable until the relevant slice lands.

### `crates/obj/`

The OMF reader/writer is the highest-value shared crate to keep
shared. Specific extensions MSC will probably push for:

- COMENT classes BCC doesn't emit but MSC does (`9F`, `9D`, and
  the precise `A1` payload variant MSC writes for default-library).
- MODEND start-address variant (`0xCA`) — start-address record
  carries a frame/target pair that the FIXUP encoder already
  handles, but the MODEND emitter probably hardcodes `0x8A`.
- A fifth (or Nth) segment in the standard table so `CONST` can be
  emitted without bespoke per-vendor wiring.

### `crates/oracle/`

The profile generalization already landed. No further changes
expected for Phase 1 — the same dosbox-driver loop runs CL.EXE
unchanged.

### `crates/fixtures/`

The `--compiler` plumbing already exists. No further changes
expected.

## Definition of done

Phase 1 closes when:

- `crates/msc/` builds, exposes a `target/debug/msc` binary, and the
  `xfix verify-all --toolchain ours --compiler msc` sweep returns
  100% byte-exact for the entire MSC corpus.
- The MSC corpus has at least 70 fixtures spanning the eight slices
  above.
- `MEMORY.md` (the auto-memory file) has entries for the MSC-specific
  patterns discovered along the way — analogous to the BCC
  publics-ordering, reg-pool-orderings, and immediate-asymmetry
  notes. The reasoning: each MSC oddity should be characterized once
  and then referenced, not re-discovered per slice.
- A `specs/msc/` subdirectory exists with per-tool discovery notes
  matching the `specs/bcc/` layout — at minimum:
    - `specs/msc/parser/` — what MSC accepts vs. BCC.
    - `specs/msc/codegen/` — frame layout, reg allocation,
      arithmetic strength reductions.
    - `specs/msc/omf.md` — the MSC-specific OMF dialect summary
      (COMENT classes, MODEND variant, segment shape).

## Open questions for Phase 1

These are questions we know we don't have answers to yet. The
shape of the answers will steer where the first MSC slices spend
time.

- **Is `__chkstk` always called, or gated by frame size?** Slice
  3's first multi-local fixture will reveal whether large frames
  switch to a different probing strategy or just call `__chkstk`
  with the larger byte count.
- **Does MSC fold constant integer expressions at parse time the
  same way BCC does?** Slice 4's first `return 1 + 2;` fixture
  will tell us.
- **Does MSC have a `register` keyword honored differently from
  BCC?** BCC promotes 1-use `register` locals; MSC may have a
  different threshold or ignore it.
- **What's the publics-ordering rule?** BCC's 80.8% rule is one of
  the harder MEMORY.md items; we don't know whether MSC's
  matches, differs, or is regular. The first multi-function fixture
  will start mapping this.
- **How does MSC's lexer treat `_`-prefixed identifiers?** Some
  versions of MSC reserve `_*` for the implementation; others
  promote user `_*` references at link time. Touches the FAQ-level
  "can I write `_foo` in source?" question.

## Phase 2 outlook

Once Phase 1 closes, Phase 2 for MSC will look like a port of the
BCC Phase 2 plan:

- Memory model expansion (medium, compact, large — huge isn't
  available in MSC 5.0).
- FP code paths (math emulator, inline 8087).
- Optimizer flags (`/O*`).
- Debug info (`/Zi` + CodeView).
- Inline `__asm`.
- DOS / runtime intrinsics that MSC's `<dos.h>` provides (some
  identical to BCC's, some different).
- Linker (`crates/link/`) — at this point we have OBJ parity and
  want EXE parity too.

But that's a future plan doc. For Phase 1 the goal is the small
self-contained corpus that proves the methodology carries.
