# TASM — the assembler

Discoveries about `TASM.EXE` go here. Suggested files:

- `DRIVER.md` — command-line surface, output naming.
- `INPUT_DIALECT.md` — the assembly dialect TASM accepts. This is largely
  the same as what `bcc -S` emits, so build this up *from* fixtures that
  pair a `.ASM` (BCC's output) with the corresponding `.OBJ` (TASM's
  output).
- `DIRECTIVES.md` — segment directives (`SEGMENT`, `ENDS`, `GROUP`, `ASSUME`),
  `PUBLIC` / `EXTRN`, `PROC` / `ENDP`, data directives, equate forms.
- `MACROS.md` — the macro language, if/when we hit it in fixtures.
- `OMF_EMISSION.md` — which OMF records TASM produces for which directives,
  with examples drawn from fixtures.

Always link discoveries back to the fixture that demonstrates them.
