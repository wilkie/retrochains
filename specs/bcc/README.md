# BCC — the C/C++ compiler (and toolchain)

Discoveries about `BCC.EXE` go here. The auxiliary tools of the Borland C++ 2.0
toolchain — assembler, linker, librarian — each have their own subdirectory:

- [`tasm/`](tasm/) — the assembler (`TASM.EXE`): asm dialect + OMF output.
- [`tlink/`](tlink/) — the linker (`TLINK.EXE`): OMF consumption, segment
  layout, fixups, MZ executable output, library resolution.
- [`tlib/`](tlib/) — the librarian (`TLIB.EXE`): `.LIB` archive layout and
  dictionary hashing.

(Cross-toolchain linker comparisons — TLINK vs MS LINK — live in
[`../linkers/`](../linkers/); shared file formats in [`../formats/`](../formats/).)

## Compiler (`BCC.EXE`) docs

Existing docs:

- [`ASM_OUTPUT.md`](ASM_OUTPUT.md) — the exact `.ASM` format BCC emits
  with `-S`: file structure, segment scaffolding, source-comment
  interleaving, codegen patterns observed so far.
- [`PARSER.md`](PARSER.md) — our lexer/parser strategy: hand-written
  recursive descent, whole-unit AST today, source-order-preserving
  emission, and fixture-driven grammar growth.
  This is *how* we plan to build the front-end, not what BCC's front-end
  does. The behavior catalog (chronological per-fixture findings) is
  split by topic under [`parser/`](parser/) — see the topic catalog in
  `PARSER.md`.

Suggested future docs (create lazily as topics emerge):

- `DRIVER.md` — command-line parsing, default flags, environment variables
  (`INCLUDE`, `LIB`), how `-I`/`-L` interact with the built-in search paths.
- `PREPROCESSOR.md` — `#include` resolution, predefined macros, the
  `__TURBOC__` / `__BORLANDC__` / `__BCPLUSPLUS__` family.
- `LEXER.md` — tokenization quirks BCC has that ANSI C doesn't.
- `SEMA.md` — type rules, implicit conversions, function-prototype handling.
- `CODEGEN.md` — how source constructs map to x86 assembly. Register
  allocation patterns. Memory-model-specific addressing.
- `OPTIMIZER.md` — what each of `-O`, `-G`, `-Z`, `-r` actually changes in
  the output, observed empirically.
- `OMF_OUTPUT.md` — when BCC emits OBJ directly (without `-S`), which OMF
  records it produces and in what order.

Always link discoveries back to the fixture that demonstrates them.
