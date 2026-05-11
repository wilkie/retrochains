# BCC — the C/C++ compiler

Discoveries about `BCC.EXE` go here. Suggested files as topics emerge
(create them lazily — don't pre-create empty stubs):

- `DRIVER.md` — command-line parsing, default flags, environment variables
  (`INCLUDE`, `LIB`), how `-I`/`-L` interact with the built-in search paths.
- `PREPROCESSOR.md` — `#include` resolution, predefined macros, the
  `__TURBOC__` / `__BORLANDC__` / `__BCPLUSPLUS__` family.
- `LEXER.md` — tokenization quirks, trigraphs, line continuation, comment
  handling.
- `PARSER.md` — accepted grammar deviations from ANSI C / pre-standard C++.
- `SEMA.md` — type rules, implicit conversions, function-prototype handling.
- `CODEGEN.md` — how source constructs map to x86 assembly. Register
  allocation patterns. Memory-model-specific addressing.
- `ASM_OUTPUT.md` — the exact `.ASM` format BCC emits with `-S`: ordering of
  directives, segment naming, comment headers, label conventions.
- `OPTIMIZER.md` — what each of `-O`, `-G`, `-Z`, `-r` actually changes in
  the output, observed empirically.
- `OMF_OUTPUT.md` — when BCC emits OBJ directly (without `-S`), which OMF
  records it produces and in what order.

Always link discoveries back to the fixture that demonstrates them.
