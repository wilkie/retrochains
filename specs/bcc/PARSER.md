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
  single-pass declaration ordering, the *exact* source-comment
  interleaving in `-S` output). Hand-rolled code lets us wedge in
  BCC-specific behaviors at the exact site they fire.
- We grow fixture-by-fixture. Adding "return integer literal" or
  "variable declaration with initializer" is a function or two of
  hand-written code each — no DSL/grammar maintenance burden.

GCC, clang, and tcc all use hand-written recursive descent for their C
parsers. There's a reason.

## Decisions

- **Single-pass.** The parser drives codegen as it goes (via event
  callbacks or a visitor), so symbols are resolved and assembly is
  emitted in source order. This mirrors how BCC actually behaved — it
  was a single-pass DOS-era compiler — and it falls out naturally for
  matching the interleaved `;` source comments / generated asm pattern
  in `-S` output (see `ASM_OUTPUT.md`).
- **Defer the lexer hack** until a fixture requires it. Until then,
  identifiers in declaration position must be one of the recognized
  primitive type keywords (`int`, `char`, `void`, ...) or it's a parse
  error. When the first `typedef`-using fixture lands, we add a
  `typedefs: HashSet<String>` to the parser and have the lexer query it.
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
├── parse/       # Hand-written recursive descent
│   ├── mod.rs       Parser struct, top-level entry points
│   ├── decl.rs      Type specifiers, declarators (K&R + ANSI)
│   ├── expr.rs      Expressions via precedence climbing
│   ├── stmt.rs      Statements
│   └── ...
├── ast/         # AST types (faithful)
├── sema/        # Symbol table, type-name classification, type rules
├── codegen/     # IR → x86 asm; emits via the writer in emit_s
├── emit_s.rs    # The .ASM-file writer (header, segments, function frame)
├── cli.rs
└── dos_time.rs
```

`codegen/` is what emits the `xor ax,ax` / `mov ax,42` / `[bp-2]` etc.
that today lives directly in `emit_s.rs`. As real codegen grows, the file
writer in `emit_s.rs` shrinks to "open file, emit header, drive codegen,
emit tail" and the operation-by-operation asm comes from the codegen
module.

## Source locations and spans

Every token carries a `Span { start: BytePos, end: BytePos }` and a
`Position { line: u32, column: u32 }` derivable from it. Every AST node
that can appear in a diagnostic carries its primary span. We need this
day one because BCC's error messages cite source positions and those
messages eventually have to match in our captured stdout/stderr (when we
care about that — currently advisory).

## Growing-the-parser order (tracked against fixtures)

The parser grows fixture-by-fixture. Each new fixture extends the parser
by exactly what that fixture needs. Anticipated short-term order:

1. **003** `return <int-literal>;` — integer-literal token, unary
   `return` statement with an integer-literal expression.
2. **004** `int <name> = <int-literal>;` — `int` type-specifier, simple
   declarator, initializer; identifier-as-rvalue in `return <name>;`.
3. Later: arithmetic expressions (precedence climbing), multiple
   declarators, function calls, control flow.

Whenever the parser refuses a construct, the verify failure should say
*why* with a clear message — that's the cue to extend the parser.

## What we explicitly defer

- Templates, namespaces, RTTI, exceptions (not in BC2.0 to relevant
  extent for our fixtures).
- The full preprocessor — for early fixtures we don't run one; sources
  have no `#include`s. When a fixture demands it, the preprocessor will
  be its own module (likely `pre/`).
- Floating-point literals, wide-char, multibyte, structs, unions, enums,
  classes — added when fixtures require.
- Error recovery for malformed input — we just bail. BCC's specific
  recovery behavior gets matched only if a fixture exercises it.
