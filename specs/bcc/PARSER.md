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

`IntLit(u32)` holds 32 bits and is wide enough for any long-sized
hex literal that's actually used in BC2 corpus. The literal's
target type is decided later by context, so a `long g = 0x12345678;`
splits across two `mov` halves the usual way (fixture `446`), and
the `L` suffix on `0xFFL` is accepted and discarded — context
already knew the target was long (fixture `447`). A long-typed
compound-assign with a hex mask (`g &= 0xFFFF;`) routes through the
same `emit_long_compound_to_mem` skeleton as any other long-with-
constant compound (fixture `448`); the hex form is purely
front-end.

## Character literals

Character constants (`'A'`, `'\n'`, `'\xFF'`) lex into
`IntLit(u32)` — C90 says character constants have type `int`, so
there's no separate `CharLit` token. The single-byte value flows
through the same paths as any other int-typed constant.

Escape sequences are shared with string literals via a single
`decode_escape` helper: `\n`, `\t`, `\r`, `\0`, `\\`, `\'`, `\"`,
`\a` (0x07), `\b` (0x08), `\f` (0x0C), `\v` (0x0B), and `\xH...`
(one-or-more hex digits, *greedily consumed*, taken mod 256).
Octal escapes (`\NNN`) and multi-byte character constants (`'ab'`)
await fixtures.

BCC's `\x` is greedy and errors with "Numeric constant too large"
when the cumulative value exceeds 0xFF — e.g. `"\x7Fb"` is parsed
as `\x7Fb` (0x7FB), not `\x7F` + `b`. Our lexer matches the
greedy parsing but silently masks to 0xFF rather than erroring;
fixtures using `\x` must keep the value in range and either end
the string immediately or follow with a non-hex character (fixture
`455` uses `"\x41"`).

Fixtures `449`–`451` cover printable ASCII (`'A'` → 65), the
common-case named escape (`'\n'` → 10), and a hex escape at the
upper end of the byte range (`'\xFF'` → 255). All three round-trip
to identical bytes vs. the equivalent decimal forms. Fixtures
`455`–`457` cover string escapes (`"\x41"`, `"\b"`, `"\f"`) — the
shared `decode_escape` works equally well in either literal form.

The store side picked up an assembler-level gap: `mov byte ptr
DGROUP:_<g>, K` (encoded `C6 06 disp16 ii`) wasn't supported. Added
as `Instr::MovGroupSymImm8`, parallel to the existing word-form
`Instr::MovGroupSymImm16` (`C7 06 ...`). The codegen side already
emitted the right asm text for char-typed globals; only the
assembler needed to learn the form.

Constant char arithmetic (`'Z' - 'A'`) folds at compile time the same
way any int-constant arithmetic does, so a char-literal pair
collapses to a single `mov word ptr DGROUP:_g, 25` (fixture `453`,
no register touched).

The compare side has an analogous byte-form: `cmp byte ptr
DGROUP:_<c>, K` (encoded `80 3E disp16 ii`), distinct from the
word-form `cmp word ptr DGROUP:_<g>, K` (`83 3E ...` / `81 3E ...`).
BCC picks the byte form when the *left* operand is a char-typed
global — even when the source-side compares against an int-promoted
char literal like `if (c == 'A')`. Added as
`Instr::CmpByteGroupSymImm8` and emitted by `emit_compare` for the
char-global branch (fixture `452`).

`switch (g)` against a global int scrutinee loads via the moffs16
short form `mov ax, word ptr DGROUP:_g` (`A1 disp16`), the same
load shape as any other int-global value-load — no special path.
Verified with character-literal case labels (fixture `454`); the
chained-compare dispatch then uses ordinary int constants since
case labels are int-typed.

## `unsigned char`

`unsigned char` is a separate type variant (`Type::UChar`) from
`char`. Storage and assignment are byte-identical to `char` —
size 1, alignment 1, same `mov byte ptr DGROUP:_c, K` encoding for
constant assignment (fixture `458`). The two diverge only on:

1. **Comparison.** Unsigned-jump mnemonic family — `jbe/jb/ja/jae`
   instead of `jle/jl/jg/jge`. `Type::is_unsigned()` includes
   `UChar`, so the existing signedness-driven jump-selection path
   picks the right mnemonic automatically. Fixture `459`:
   `if (c > 200)` becomes `cmp byte ptr [_c], 200 / jbe …`.
2. **Int promotion.** Zero-extend via `mov ah, 0` (`B4 00`, 2 bytes)
   instead of sign-extend via `cbw` (`98`, 1 byte). Driven by
   `gty.is_unsigned()` at the load site. Fixture `460`:
   `g = c + 1` becomes `mov al, [_c] / mov ah, 0 / inc ax /
   mov [_g], ax`.

Since storage is shared, almost every `matches!(<t>, Type::Char)`
site in the codegen was a storage-width check (`byte` vs. `word`).
Those were converted en masse to `<t>.is_char_like()` which returns
true for both `Char` and `UChar`. Sign-extension sites stayed
explicit and consult `is_unsigned()` to pick between `cbw` and
`mov ah, 0`.

Parser: the top-level type-probe and `parse_type` both learned the
`unsigned char` sequence. `unsigned` followed by `char` consumes
both tokens and yields `Type::UChar`; the existing post-`unsigned`
`int`-consumption stays intact for the `unsigned int` and bare
`unsigned` forms.

### Coverage in locals, struct fields, arrays

Fixtures `461`–`463` extend uchar to stack-local, struct-field, and
array contexts. The codegen now factors the widening choice into a
single `emit_widen_al(&ty)` helper — applied at every char-load
site (`mov al, byte ptr <addr>` followed by widening). Each site
threads through the leaf type (`leaf_ty`, `pointee`, `field_ty`)
so signed `char` keeps `cbw` and `unsigned char` picks
`mov ah, 0`. The local-assign path also gained a char-aware
immediate-store shape: `mov byte ptr [bp-N], K` (`C6 46 ii ii`, 4
bytes) instead of the previously-unconditional `mov word ptr
[bp-N], K` (`C7 46 ii ww ww`, 5 bytes) for stack-resident char
locals.

### Struct sizing — raw vs. rounded

Fixture `462` (`struct { unsigned char b; int x; }` in `_BSS`)
pinned an unobserved corner: **BCC's struct intrinsic size is the
raw field-sum, with no end-padding to a word boundary**. `_s`
emits 3 bytes of BSS storage, not 4. The earlier "round size up to
even" rule in `parse_record_type` was wrong — it conflated
intrinsic size with local-frame allocation size. Removed the round
for structs (kept for unions until a fixture pins their behavior).

To preserve fixture `102`'s `[bp-4],9` / `[bp-3],42` layout, the
*local frame allocator* now rounds each slot's size up to the
type's alignment before adding it to the running `stack_bytes`
cursor. So a 3-byte struct with alignment 2 occupies a 4-byte
slot, with the struct base at the low (aligned) address and
field offsets layered on top. The previous code already aligned
the *start* of each slot to the type's alignment; rounding the
size up to alignment as well closes the gap.

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
