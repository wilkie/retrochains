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

## Topic catalog

The behavior catalog is split by topic into [`parser/`](parser/). Each file is the chronological log of fixtures that exercised that topic.

- [`parser/compound-assigns.md`](parser/compound-assigns.md) — Compound assignments (107 sections)
- [`parser/arrays-pointers.md`](parser/arrays-pointers.md) — Arrays and pointers (91 sections)
- [`parser/control-flow.md`](parser/control-flow.md) — Control flow (if/while/for/goto/return/&&/||/ternary) (58 sections)
- [`parser/long-codegen.md`](parser/long-codegen.md) — `long` / `unsigned long` codegen (44 sections)
- [`parser/shifts.md`](parser/shifts.md) — Shifts and rotates (41 sections)
- [`parser/structs.md`](parser/structs.md) — Structs, unions, members, typedefs (40 sections)
- [`parser/arithmetic.md`](parser/arithmetic.md) — Arithmetic codegen (mul/div/mod/peepholes/identity folds) (38 sections)
- [`parser/helpers.md`](parser/helpers.md) — Runtime helpers (`N_LXMUL@` family etc.) (33 sections)
- [`parser/free-passes.md`](parser/free-passes.md) — Free passes (batches that needed no codegen changes) (26 sections)
- [`parser/switch.md`](parser/switch.md) — Switch dispatch (23 sections)
- [`parser/char-codegen.md`](parser/char-codegen.md) — `char` / `unsigned char` codegen (19 sections)
- [`parser/calling-conventions.md`](parser/calling-conventions.md) — Calling conventions (17 sections)
- [`parser/float-codegen.md`](parser/float-codegen.md) — `float` / `double` codegen (17 sections)
- [`parser/types-qualifiers.md`](parser/types-qualifiers.md) — Types and qualifiers (14 sections)
- [`parser/operators.md`](parser/operators.md) — Operators, evaluation order, enregistration (10 sections)
- [`parser/bitwise.md`](parser/bitwise.md) — Bitwise operations (9 sections)
- [`parser/declarations.md`](parser/declarations.md) — Declarations and storage layout (9 sections)
- [`parser/memory-models.md`](parser/memory-models.md) — Memory models (8 sections)
- [`parser/bitfields.md`](parser/bitfields.md) — Bitfields (7 sections)
- [`parser/flags-and-preprocessor.md`](parser/flags-and-preprocessor.md) — Compiler flags and preprocessor (7 sections)
- [`parser/literals.md`](parser/literals.md) — Literals (integer, character, string, sizeof) (6 sections)

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
