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

## Not yet built — standalone-linker bucket

The purest linker tests drive `TLINK`/`LINK` **directly** on hand-built object
files (custom segment classes/orders, `.DEF` exports, overlays, library member
selection, malformed-OBJ diagnostics) — things the compiler driver won't emit.
That needs an input `.OBJ`, which raises one open decision before we build it:

- **Track input `.OBJ`s** under `fixtures/c/linking/standalone/` (allowed by
  `.gitignore`, which only prunes OBJs at the repo root + the `artifacts/`
  cache), pairing each with the `.ASM`/`.C` + command it was built from for
  provenance; **or**
- **add a cross-tool chain** to the harness (assemble `.ASM`→`.OBJ` with
  `tasm`/`masm`, then link) so the tracked input stays plain-text source.

`run_chained` is currently one-tool-per-session, so the second option is a
harness change. Pick one before adding `tool = "tlink"` / `tool = "link"`
fixtures; until then they'd be **not-applicable** under `--toolchain ours`
(no linker reimplementation to run).
