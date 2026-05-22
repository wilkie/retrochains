# Literals (integer, character, string, sizeof)

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

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

## Adjacent string literal concatenation

Fixture `508` (`char *s = "Hello, " "World";`) — C90 specifies
that adjacent string-literal tokens concatenate at translation
time. `parse_primary`'s StringLit arm now peeks for further
StringLit tokens, appending their bytes (and extending the
span) before returning a single `ExprKind::StringLit`. The
combined literal flows through the existing pool path, so
nothing downstream needed to change.

## `sizeof` of a string literal

Fixture `511` (`return sizeof("hi");`) — extended
`expr_static_size` to handle `ExprKind::StringLit`. The result is
`bytes.len() + 1` to include the NUL terminator. The expression
folds to an `IntLit` at parse time, just like `sizeof(<type>)`.

## Adjacent string literals concatenated at parse time; numeric bases all → int; `\0` in literal ≠ terminator

Fixtures `2237` (string concat), `2238` (dec/hex/
oct literals), `2239` (escape sequences) cover
lexer-side text literal handling.

- `2237` (**adjacent string literal concat**):
  `"hello" " " "world"` → single string "hello
  world\0" in `_DATA`. Parser-level
  concatenation, zero runtime cost. C89 standard
  behavior.
- `2238` (**numeric literal bases**): all three
  resolve to the same internal int value at
  parse time:
  - `100` (decimal) → 0x64
  - `0x64` (hex, leading `0x`) → 0x64
  - `0144` (octal, leading `0`) → 0x64
  
  All three stores emit `c7 46 disp 64 00` (mov
  imm16 = 100). No base distinction in codegen.
- `2239` (**escape sequences in char arr init**):
  `"a\nb\tc\0d"` stored as:
  ```
  61 0a 62 09 63 00 64 00     ; 8 bytes
  'a' \n  'b' \t  'c' \0  'd' \0
  ```
  Key observation: `\0` is just a byte (0x00),
  NOT a terminator for the array initializer.
  The literal continues with 'd' after the
  embedded null. The trailing null is the
  standard C string terminator.

**Escape sequence catalogue** (BCC 2.0 recognised):
| Escape | Value | Meaning |
|--------|-------|---------|
| `\n` | 0x0A | newline |
| `\t` | 0x09 | tab |
| `\r` | 0x0D | carriage return |
| `\0` | 0x00 | null (NOT terminator in literal) |
| `\\` | 0x5C | backslash |
| `\'` | 0x27 | single quote |
| `\"` | 0x22 | double quote |
| `\a` | 0x07 | alert/bell |
| `\b` | 0x08 | backspace |
| `\f` | 0x0C | form feed |
| `\v` | 0x0B | vertical tab |
| `\xHH` | 0xHH | hex escape (probable) |
| `\NNN` | octal | octal escape (probable) |

**Numeric literal recognition**:
| Prefix | Base |
|--------|------|
| (default, leading nonzero) | 10 (decimal) |
| `0` (leading zero, NOT `0x`) | 8 (octal) |
| `0x` or `0X` | 16 (hex) |
| `0b` or `0B` | (NOT supported — C99+ only) |
| Suffix `L`/`l` | long |
| Suffix `U`/`u` | unsigned |
| Suffix `F`/`f` | float |
| (no suffix on float) | double |

For the Rust reimplementation:
- Lexer: recognize and concatenate adjacent
  string literals.
- Lexer: parse numeric bases, return canonical
  int/long/float value.
- Lexer: handle escape sequences in char and
  string literals.
- Embedded `\0` does NOT terminate the literal
  body (only the trailing null does).

## Boundaries: `x ^ x`/`x - x` NOT folded; `x & 0xFFFF` NOT folded — only literal-0/1 ops folded

Fixtures `2015` (x ^ x), `2016` (x - x), `2017`
(x & 0xFFFF) probe the **boundaries** of BCC's
identity-folding.

- `2015` (**`x ^ x` NOT folded**): emits literal
  `xor ax, si` (`33 c6`). BCC does not recognize
  same-variable optimization (would require
  variable-identity tracking).
- `2016` (**`x - x` NOT folded**): emits literal
  `sub ax, si` (`2b c6`). Same reason.
- `2017` (**`x & 0xFFFF` NOT folded**): emits
  literal `and ax, 0xFFFF` (`25 ff ff`). Even
  though 0xFFFF is the identity-mask for 16-bit
  AND, BCC doesn't recognize it.
  
  Notable: the AND with 0xFFFF is a no-op for
  int, but BCC still emits it. So **`x & -1`
  also NOT folded**.

**Refined identity-folding rule** (boundary
clarified):
| Pattern | Folded? |
|---------|---------|
| `x + 0`, `0 + x` | YES |
| `x - 0` | YES |
| `x * 1`, `1 * x`, `x / 1` | YES |
| `x | 0`, `x ^ 0` | YES |
| `x * 0` | YES (folds to 0) |
| `x ^ x`, `x - x` | NO (same-var not tracked) |
| `x & 0xFFFF`, `x & -1` | NO (only literal 0/1 patterns recognized) |
| Any expression of compile-time constants | YES (full constant folding) |

So the identity-folding catalog is **strictly
literal-0/1 based**:
- For additive/bitwise ops: only literal 0
- For multiplicative ops: only literal 1 (and 0
  for *)

For the Rust reimplementation:
- Implement identity folding for literal 0 and
  literal 1 patterns only.
- Do NOT attempt variable-identity simplification
  or all-ones-mask recognition.

