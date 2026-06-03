# Second-compiler integration plan

This document captures the design choices for adding a second (and
eventually third) reimplementation target alongside Borland C++ 2.0.
The intent is to keep the byte-exact reimplementation methodology
intact while sharing as much of the harness, fixture corpus, and OMF
machinery as possible.

The leading candidate for the second target is **Microsoft C 5.0 /
5.1** (1987 / 1988). Watcom C 9.x or 10.x is a serious alternative
because its OMF dialect (with EASY-OMF 32-bit extensions) would force
more generalization of `crates/obj` up front. The plan below tries to
stay agnostic between those two options.

## Why single-repo (recap)

We already chose single-repo, side by side. The reasons:

- `crates/obj`, `crates/oracle`, the `xfix` harness, and the
  `fixtures/` corpus are all worth one copy, not N copies.
- Cross-compiler comparisons (`bcc` picks AX where `msc` picks DX)
  are easier to investigate when both live in the same tree.
- The fixture's C source is the durable artifact; per-compiler goldens
  are a natural fan-out from that.

Cost: repo growth. We're already at ~3900 fixtures and the tree
remains healthy; doubling or tripling the goldens is tolerable.

## Project layout — proposed

```
crates/
  oracle/                     # Tool enum already exists; widen it
  fixtures/                   # xfix harness — needs per-compiler awareness
  obj/                        # OMF reader/writer — shared
  tasm/                       # BCC's asm dialect (TASM-flavor)
  masm/                       # MSC's asm dialect (added when second target lands)
  tlink/                      # BCC's linker
  link/                       # MSC's linker (LINK.EXE) (added when needed)
  bcc/                        # the BCC reimplementation (unchanged)
  msc/                        # the MSC reimplementation (added)
  bcc-wasm/, msc-wasm/        # per-compiler wasm bundles
fixtures/
  001-hello/
    HELLO.C
    invocation.bcc.toml       # one per compiler that runs this fixture
    invocation.msc.toml       # (may be absent if the fixture is BCC-only)
    expected/
      bcc/                    # per-compiler goldens
        HELLO.OBJ
        manifest.toml
        stderr
        stdout
      msc/
        HELLO.OBJ
        manifest.toml
        stderr
        stdout
```

The `expected/<compiler>/` fan-out and the `invocation.<compiler>.toml`
naming let a fixture opt into multiple compilers independently. A
fixture that uses BCC-specific syntax (`asm { ... }` blocks with
`_AX`-style passthrough, or BCC-only headers) just omits the MSC
invocation file and the verify pass skips it for that compiler.

## Migration plan (no-op first)

Touching ~3900 fixtures should be a single deterministic rename:

1. **Pre-migration**: tag the current state (`v-phase2-end` or
   similar) so the migration is bisectable.
2. **Filesystem rename**:
   - `fixtures/*/invocation.toml` → `fixtures/*/invocation.bcc.toml`
   - `fixtures/*/expected/HELLO.OBJ` →
     `fixtures/*/expected/bcc/HELLO.OBJ`, same for `manifest.toml`,
     `stderr`, `stdout`.
   - Anything else under `expected/` follows the same fan-out.
3. **Harness update**: `xfix` learns to pick `invocation.<compiler>.toml`
   and `expected/<compiler>/` by an explicit `--compiler` flag
   (defaulting to `bcc` so existing scripts and muscle memory keep
   working).
4. **Corpus regression**: re-run the full sweep, verify byte-exact
   parity with the pre-migration tag for every BCC golden. If
   anything diverged, the rename had a bug — fix the rename, don't
   re-capture.
5. **Commit the migration as a single change** so `git blame` keeps
   pointing at the codegen change rather than the rename for the
   actual goldens.

After the rename lands and the corpus is clean, the second compiler
is purely additive — no existing files change.

## `invocation.<compiler>.toml` schema

Existing BCC schema:

```toml
tool = "bcc"
args = ["-c", "-ms", "HELLO.C"]
inputs = ["HELLO.C"]
```

After migration, each invocation file is per-compiler. The `tool`
field is still the driver binary, but now distinct values mean
different real-world toolchains:

```toml
# invocation.bcc.toml
tool = "bcc"
args = ["-c", "-ms", "HELLO.C"]
inputs = ["HELLO.C"]
oracle = "borland-bc2"            # which oracle profile to drive

# invocation.msc.toml
tool = "cl"                       # MSC's driver
args = ["/c", "/AS", "HELLO.C"]   # /AS = small model in MSC
inputs = ["HELLO.C"]
oracle = "microsoft-msc5"
```

Open question: should `oracle` be implicit from the file suffix
(`invocation.bcc.toml` ⇒ `borland-bc2` oracle) or explicit? Implicit
is less repetition; explicit is more flexible (e.g. running BCC under
a different DOS image). I lean implicit for now, explicit as
escape hatch when needed.

The harness needs a registry of oracle profiles. Each profile knows:

- DOS image / ZIP path
- driver binary path inside the image
- environment variables (`INCLUDE`, `LIB`, `PATH`)
- file-name munging (DOS 8.3 + case rules)
- faketime anchor (BCC sets `1991-04-23T12:00:00Z`; MSC 5 should be
  set to an MSC-era timestamp so OMF mtime fields don't clash)

## Capture / verify command surface

```
xfix capture <fixture>                       # all compilers that have invocation files
xfix capture --compiler bcc <fixture>         # just one
xfix verify --toolchain ours --compiler msc <fixture>
xfix verify --toolchain oracle <fixture>      # both compilers, if both files present
```

Default behavior when no `--compiler` is given: run every compiler that
has an `invocation.<X>.toml` file. This makes the corpus-wide sweep do
the right thing without per-fixture configuration.

The "ours" toolchain (the host-side reimplementation) is also per-
compiler: `--toolchain ours --compiler bcc` runs `crates/bcc`,
`--toolchain ours --compiler msc` runs `crates/msc`.

## What stays shared

- **`crates/obj`**: OMF reader/writer. May need extensions (more
  record types — MSC 5 might emit different optional records than
  BC2). Watcom would force LIDATA, COMENT subrecord differences, and
  possibly EASY-OMF 32-bit records to be supported.
- **`crates/oracle`**: the DOSBox + faketime wrapper. Generalize
  `Tool` enum to a `(Vendor, Tool)` pair or a free-form profile
  identifier.
- **`crates/fixtures`** (the `xfix` harness): the capture/verify
  contract, byte-diff format, manifest schema, sha256 hashing.
- **Format-aware diff machinery**: the "OMF structural diff" output
  that we lean on for triaging mismatches.

## What forks

- **`crates/tasm` vs `crates/masm`**: MASM and TASM share a heritage
  but have meaningful syntactic differences (segment ordering rules,
  `OPTION` directive, `ASSUME` semantics, `dup (?)` vs `dup ?`,
  procedure scoping). Trying to make one parser handle both is a
  premature merge. Sibling crates are clearer.
- **`crates/bcc/codegen` vs `crates/msc/codegen`**: every compiler's
  IR-to-OMF pipeline is its own beast. Sharing an IR is premature
  abstraction — they would diverge under any non-trivial feature.
- **Memory model conventions**: MSC's small / medium / large / compact /
  huge map onto the same OMF segment shapes BCC uses, but the
  segment names differ (`_TEXT` vs `<MODULE>_TEXT`, `DGROUP` formation
  rules, far data class names). The huge-model rewrite pass would be
  parallel but separate.
- **Per-compiler memories**: publics ordering, register pool orderings,
  immediate-form asymmetries, etc. Every compiler discovers its own
  set; we should keep them in compiler-specific memory files.

## MSC 5.0 / 5.1 specifics worth front-loading

Things that will likely surface within the first few MSC fixtures:

1. **Driver tool name**: `cl.exe` drives `c1.exe` / `c2.exe` /
   `c3.exe` (compiler passes) plus `masm.exe` and `link.exe`. The
   oracle needs to drive `cl` as the single entry point and let it
   shell out internally, just like we drive `bcc` (which itself
   shells out to `tasm` and `tlink`).
2. **Output file naming**: `cl /c HELLO.C` writes `HELLO.OBJ` (same
   as BCC's `-c`). Good — same fixture file shape works.
3. **OMF differences vs. BC2**:
   - MSC 5 may not emit COMENT class A0 (memory-model hint) the way
     BCC does — or emits a different value.
   - MSC 5 emits `LEDATA` and `FIXUPP` records like BCC but the
     PUBDEF / EXTDEF ordering rules are different — the BCC publics-
     ordering memory does NOT carry over.
   - MSC may emit `THEADR` with different name conventions (no
     `.OBJ` extension; different case rules).
   - Library and helper symbols are different (`_chkstk` vs BCC's
     `__brklvl`-based stack frame).
4. **Runtime library**: MSC ships `SLIBCE.LIB` / `SLIBC7.LIB` /
   `MLIBC7.LIB` etc. depending on model + 8087 emulation. The
   capture environment needs the right one on `LIB`.
5. **Header files**: `<stdio.h>`, `<string.h>` etc. exist in both
   but MSC's macro definitions and FAR pointer type defaults differ.
   The first MSC fixtures should stick to flag-only invocations
   (no `-c` against fixtures that include `<dos.h>`) until the
   header path is sorted.
6. **Inline asm**: MSC uses `__asm { ... }` with semicolon line
   separators rather than BCC's `asm { ... }` block syntax. Some
   BCC inline-asm fixtures will not port directly.
7. **Function-call ABI**: both compilers default to cdecl with
   right-to-left push order; that's portable. But MSC introduces
   `_pascal` keyword (Pascal calling convention) earlier than BCC
   and uses it inside Windows headers — not relevant for now but
   would be relevant if we expand to Windows OBJ.

## Watcom-as-alternative considerations

If we picked Watcom C 9.x or 10.x instead of MSC:

- Watcom drives `wcc` (16-bit) / `wcc386` (32-bit) directly; no
  unified `cl`-style driver in early versions.
- Watcom emits standard OMF but with extensions (LIDATA, COMENT
  classes A1/A3) that BC2 doesn't use heavily. `crates/obj` would
  grow.
- Watcom supports register-based calling conventions (`__watcall`)
  that produce visibly different codegen — this would be a strong
  test of how compiler-agnostic our IR generation patterns are.
- Watcom's 32-bit extension (`wcc386`) would push toward 32-bit OMF,
  which is genuinely different territory (LXDATA/LXDEF, 32-bit
  fixups). Could be deferred to a third compiler.

Trade-off: MSC 5 is simpler (16-bit only, classic OMF), Watcom is
more interesting (forces real generalization, has unique calling
conventions). For ramping up the methodology, MSC 5 is probably the
better second target.

## Fixture porting strategy

After the layout migration:

1. Walk the corpus and identify fixtures that are BCC-specific.
   Likely categories:
   - Inline `asm { ... }` blocks with BCC-specific syntax.
   - `<dos.h>` intrinsics that differ across compilers.
   - K&R-style declarations that MSC may reject.
   - `huge` keyword (MSC 5 might not support it the same way).
   - `__brklvl` references and BCC-specific runtime symbols.
2. Mark those fixtures as `bcc`-only by not creating
   `invocation.msc.toml`. The harness skips them silently for MSC.
3. Run `xfix capture --compiler msc` against everything else. Some
   will syntactically fail to compile under MSC; those need either a
   fixture-source tweak or an explicit skip.
4. The captured MSC goldens then become regression anchors for the
   parallel `crates/msc` reimplementation.

Expectation: 50–70% of existing fixtures will produce valid MSC
output without source changes. The rest split into:

- Genuinely compiler-specific syntax (skip for MSC).
- Source quirks that minor edits would fix (worth the edit).
- Cases where MSC's behavior is identical at the C level but the
  OBJ output is structurally different — these are the meat of the
  MSC codegen reimplementation.

## Cross-compiler comparison reporting

A new output worth building: per-compiler corpus dashboards.

```
$ xfix status
3917 fixtures total
  bcc:  3917 capture-able, 3917 byte-exact (100.0%)
  msc:  2541 capture-able, 0 byte-exact (0.0%, msc reimpl not yet started)
```

Per-fixture, a verify report can show whether the fixture matches
under bcc, msc, both, or neither — useful for triaging codegen
divergence patterns.

## Definition of done (for the migration alone)

- All ~3900 existing fixtures migrated to the new layout.
- `xfix capture` and `xfix verify` accept `--compiler` and default to
  doing the right thing across all compilers.
- Corpus regression sweep passes byte-exact for BCC after the rename.
- `crates/bcc` still builds and runs identically.
- Plan-of-record updated to recommend the per-compiler invocation
  files as the long-term shape.

The migration itself is the smallest valuable move. It commits to
nothing about which second compiler we pick, and it unlocks parallel
compiler work whenever we're ready.

## Definition of done (for the first MSC-target slice)

- `crates/msc` exists with a single hello-world fixture passing
  byte-exact.
- The oracle has a `microsoft-msc5` profile (or similar) that drives
  `cl.exe` inside DOSBox with the matching `INCLUDE` / `LIB` paths.
- A Phase 3 plan doc (`MSC_PHASE_1.md` or similar) defines the
  bring-up corpus, mirroring this repo's existing Phase 1 plan for
  BCC.
- The cross-compiler comparison report (`xfix status` or equivalent)
  shows two columns: bcc and msc.

## Open questions

- **License / redistribution**: neither BC2.zip nor MSC500.zip is ours
  to redistribute, so neither is committed — we track a `.sha256`
  integrity manifest plus a `.md` how-to-acquire doc for each, and the
  developer drops their own zip at a configurable path the oracle reads.
- **Multi-compiler CI**: the corpus regression is already slow;
  running it under N compilers multiplies the time. Worth thinking
  about parallel oracle invocations or compiler-conditioned subsets.
- **Tooling overlap**: does the second compiler want its own version
  of `crates/tlink` for linking, or do we just trust the vendor's
  `LINK.EXE` until we have a reason to reimplement it?
- **Inline asm dialects**: BCC's `asm { ... }` block and MSC's
  `__asm { ... }` block differ. Should the fixture C source be
  preprocessor-gated (`#ifdef __BORLANDC__`) to keep one source for
  multiple compilers, or should we keep separate sources per
  compiler? Preprocessor gating keeps one source of truth but makes
  fixtures less readable. Separate sources are more honest about
  what each compiler is consuming.
- **What's "byte-exact" mean across compilers?**: each compiler's
  goldens are anchored against its own oracle. Cross-compiler byte
  equality is not a goal — only intra-compiler byte equality is.
  Worth stating explicitly so the dashboards don't confuse the
  reader.

## Suggested ordering when this work resumes

1. Land the layout migration (no-op for BCC).
2. Decide MSC vs. Watcom; document the choice.
3. Add the chosen oracle profile + a hello-world fixture.
4. Build out the bring-up corpus for the second compiler.
5. Start the second-compiler reimplementation slice.

Steps 1 and 2 are the only ones that block further BCC work — once
they land, BCC and the second compiler can progress in parallel.
