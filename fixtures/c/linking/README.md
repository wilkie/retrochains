# `linking/` — linker-intrinsic fixtures

Goldens for behavior a **single translation unit can't reach**: cross-module
symbol resolution, segment/group combination across object files, and the
linker's `.EXE`/`.MAP` output conventions. Feeds the linker reimplementation
(`crates/bcc-tlink`, future `crates/msc-link`) and EXE-level fingerprinting
(`specs/linkers/DIFFERENCES.md`). See `specs/plans/LINKER_FIXTURES.md` for the
overall plan.

## How these gate (important)

Our reimplementation today is the **compiler** (`bcc`, `cl`/`msc`), not the
linker — `tlink`/`link` are stubs. So under `--toolchain ours` the gated,
byte-exact contract for a linking fixture is the **per-module `.OBJ`s** the
compiler emits; the linked `.EXE` and `.MAP` are recorded goldens but treated as
**advisory** (non-gating) until a linker lands. This mirrors how `.ASM` listings
are advisory. The advisory carve-out lives in `verify_ours`
(`is_advisory_output_name`: `.ASM`/`.EXE`/`.MAP`).

Mechanically each fixture is **compiler-driven**: the primary `args` compile
every module (`bcc -c -ms M1.C M2.C …` / `cl /c /AS M1.C M2.C …`), which our
compiler runs and we verify OBJ-for-OBJ; the `link_args` pass re-drives the
*same* driver in link mode (`bcc -M -LC:\LIB …` / `cl /AS /Fm …`) so the oracle
also records the `.EXE` and `.MAP`. Both our `bcc` and `cl` accept multiple
source files and emit one OBJ per TU, so multi-module fixtures gate the compiler
on **each** TU as a bonus.

## Layout

- **`multi-module/`** — 2+ tracked `.C` modules compiled and linked together.
  Reaches cross-TU public/extern resolution, segment combination ordering, and
  real `.MAP`s from both linkers:
  - `4254-call-across-modules` — `main()` calls an extern function in another TU.
  - `4255-global-across-modules` — a TU reads/writes an extern `int` defined
    in another (cross-TU data symbol + DGROUP combination).
  - `4256-three-module-chain` — `main→b→c` across three OBJs (transitive
    resolution, `_TEXT` combination order across three modules).
  - `4257-common-tentative-def` — same uninitialized global tentatively defined
    in two TUs. **MSC** emits communal `COMDEF`s that MS LINK merges (links
    clean). **BCC** emits definite publics that collide → TLINK duplicate-symbol
    error, so the BCC sibling captures the *compile* stage only. The divergence
    itself is the fingerprint (catalogued in `specs/linkers/DIFFERENCES.md`).

## `standalone/` — drive the linker directly on hand-built OBJs

The purest linker tests drive `TLINK`/`LINK` **directly** on hand-built object
files — isolating the linker's own behavior (MZ-image conventions, cross-module
symbol resolution and fixups, segment classes/orders, and later `.DEF` exports,
overlays, library member selection, malformed-OBJ diagnostics) from any
compiler-driver defaults. The input is an `.OBJ`, so:

- `tool = "tlink"` / `tool = "link"`, `inputs = ["MOD.OBJ", …]`, and `args` is the
  raw linker command (e.g. `["/m", "MINI.OBJ,MINI.EXE,MINI.MAP"]`).
- The `.OBJ` is **tracked** as the fixture input. `.gitignore` only prunes OBJs
  at the repo root and under `artifacts/`, so OBJs in the fixtures tree are
  trackable — and a linker fixture's input OBJ is *source*, not a reproducible
  compiler golden. Each OBJ is paired with the `.ASM` it was assembled from
  (tracked alongside, for readability + regeneration) but the `.ASM` is **not**
  in `inputs`, so the harness only materializes the OBJ.

### Building one (the provenance workflow)

Two oracle passes; the second overwrites the first's manifest:

1. Write `MOD.ASM`. Temporarily set the invocation to assemble it
   (`tool = "tasm"`, `args = ["MOD.ASM"]`, `inputs = ["MOD.ASM"]`; assemble
   several at once with `asm_args` since both passes run the same tool), then
   `xfix capture` + `xfix materialize`. Copy the resulting
   `artifacts/.../bcc/BC2/MOD.OBJ` into the fixture as the tracked input.
2. Rewrite the invocation to `tool = "tlink"` with the link `args` and
   `inputs = ["MOD.OBJ", …]`, then `xfix capture`. The recorded outputs are now
   `MOD.EXE` + `MOD.MAP`.

Seeded examples:
- `4258-tlink-minimal-exe` — one OBJ, no runtime → pins TLINK's MZ-header
  conventions (checksum left zero, reloc packing, default SP/min-alloc).
- `4259-tlink-extrn-public` — `MAIN.OBJ` `CALL`s an `EXTRN PROC` that `SUB.OBJ`
  exports `PUBLIC` → pins cross-module resolution + near-call fixup.

### How these gate

`tlink`/`link` have no host reimplementation yet, so under `--toolchain ours`
these fixtures are **skipped** (`HarnessError::ToolNotImplemented` → the
`verify-all` "skipped" tally, `NotApplicable` on the dashboard), while the oracle
still captures and verifies their goldens. The day a real linker lands in
`crates/bcc-tlink` (bind it in `ToolPaths::from_workspace_debug`), they light up
and **gate the `.EXE`** — `is_advisory_output` already routes `.EXE` to gating
for linker-tool fixtures (the `.MAP` stays advisory until byte-exact map
generation is a deliberate milestone). That flip is the start of the linker
implementation pass.
