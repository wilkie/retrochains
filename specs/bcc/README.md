# BCC — the C/C++ compiler

Discoveries about `BCC.EXE` go here.

Existing docs:

- [`ASM_OUTPUT.md`](ASM_OUTPUT.md) — the exact `.ASM` format BCC emits
  with `-S`: file structure, segment scaffolding, source-comment
  interleaving, codegen patterns observed so far.
- [`PARSER.md`](PARSER.md) — our lexer/parser strategy: hand-written
  recursive descent, whole-unit AST today, source-order-preserving
  emission, and fixture-driven grammar growth.
  This is *how* we plan to build the front-end, not what BCC's front-end
  does.

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
