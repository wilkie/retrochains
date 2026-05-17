# BCC lexer + parser strategy

## Approach: hand-written recursive descent

The lexer and parser are hand-written. We don't use a parser generator
(LALR/PEG/combinators) for the standard reasons every production C
compiler avoids them:

- C is famously context-sensitive — the canonical example being `T * x;`
  which is a declaration when `T` is a typedef-name and an expression
  statement otherwise. Resolving this requires the parser to feed
  classification back to the lexer (the "lexer hack"), which any parser
  generator must be wedged into supporting.
- Byte-exact matching of `BCC.EXE` requires preserving BCC's
  idiosyncrasies (warning text, error positions, K&R acceptance,
  declaration ordering, the *exact* source-comment interleaving in
  `-S` output). Hand-rolled code lets us wedge in BCC-specific
  behaviors at the exact site they fire.
- We grow fixture-by-fixture. Adding "return integer literal" or
  "variable declaration with initializer" is a function or two of
  hand-written code each — no DSL/grammar maintenance burden.

GCC, clang, and tcc all use hand-written recursive descent for their C
parsers. There's a reason.

## Decisions

- **Whole-unit AST today; source-order emission preserved.** The current
  implementation tokenizes, parses a full translation-unit AST, and then
  codegen walks that AST in source order. This is not a literal parser-
  drives-codegen pipeline, but it preserves the ordering that matters for
  BCC-style source comments, function labels, and symbol emission.
- **Typedef classification is parser-side today.** The first typedef
  fixtures have landed, so `Parser` now carries a typedef table and treats
  matching identifiers as type names in declaration/type contexts. There
  is not a separate lexer callback API; the lexer still produces ordinary
  identifiers and the parser classifies them where needed.
- **AST faithful to source order.** Preserve the order BCC saw the
  declarations and statements in. Preserve redundant parentheses and
  comment positions to the extent they affect output. We can normalize
  later if/when an optimizer wants it, but byte-exact reproduction of an
  ordering-sensitive compiler depends on not normalizing in front-end.

## Module layout (inside `crates/bcc/`)

```
src/
├── lex/         # Lexer
│   ├── mod.rs       Lexer struct, public API
│   ├── token.rs     Token enum + Span
│   └── ...
├── parse/       # Hand-written recursive descent (currently in mod.rs)
│   └── mod.rs       Parser struct, top-level items, declarations,
│                    statements, expressions, typedef/record tables
├── ast/         # AST types (faithful)
├── codegen/     # AST → x86 asm; emits via the writer in emit_s
├── emit_s.rs    # The .ASM-file writer (header, segments, function frame)
├── emit_obj.rs  # Direct -c path, using TASM/OMF support
├── cli.rs
└── dos_time.rs
```

`emit_s.rs` owns file-level scaffolding (macro preamble, segment
scaffold, globals/string tail), while `codegen/` owns function bodies and
most instruction-level BCC patterns.

## Source locations and spans

Every token carries a `Span { start: BytePos, end: BytePos }` and a
`Position { line: u32, column: u32 }` derivable from it. Every AST node
that can appear in a diagnostic carries its primary span. We need this
day one because BCC's error messages cite source positions and those
messages eventually have to match in our captured stdout/stderr (when we
care about that — currently advisory).

## Growth Model

The parser still grows fixture-by-fixture. Early fixtures covered integer
returns, local declarations, arithmetic, calls, and control flow; later
fixtures added typedefs, structs/unions, enums, static locals,
K&R-parameter declarations, casts, `sizeof`, pointers, arrays, function
pointers, and `long`/`unsigned long` spellings. The document should not be
read as a complete C grammar: implemented grammar exists only where a
fixture has forced it.

Whenever the parser refuses a construct, the verify failure should say
*why* with a clear message. That failure is the cue to capture the
smallest oracle fixture for the construct before extending the parser.

## Integer literal forms

C90 spells integer literals three ways and the lexer accepts all
three:

- `0x`/`0X` prefix → hex (`0xFF`, `0X1234`).
- Bare leading `0` followed by an octal digit → octal (`0755` is 493,
  *not* 755 — verified by fixture `445` whose oracle bytes encode
  0x01ED).
- Otherwise → decimal.

A lone `0` is decimal zero — the octal check requires a trailing
`0..=7` digit. Suffixes `L`/`l`/`U`/`u` and combinations are accepted
and discarded as before; the surrounding type context decides the
ultimate width. Fixtures `443`–`445` round-trip hex assignment, hex
mask via compound-assign, and an octal literal respectively.

Hex/octal and decimal codegen are *equivalent at the byte level* —
`s.x &= 0xFF` and `s.x &= 255` produce identical OBJs (this was
empirically verified during fixture `390`'s capture). So adding the
lexer support didn't require any codegen changes; the literal value
flows through `IntLit(u32)` regardless of source form.

## What we explicitly defer

- Templates, namespaces, RTTI, exceptions (not in BC2.0 to relevant
  extent for our fixtures).
- The full preprocessor — current fixtures avoid `#include` and macro
  expansion. When a fixture demands it, the preprocessor should be its own
  module.
- Floating-point literals, wide-char, multibyte, C++ classes, templates,
  exceptions, and full C/C++ diagnostic recovery.
- Error recovery for malformed input — we just bail. BCC's specific
  recovery behavior gets matched only if a fixture exercises it.
