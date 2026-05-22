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


## String literal subscript — folds to `mov al, [disp16]` (fixture `2381`)

`"ABCDEF"[2]` — constant subscript of a string literal compiles to a
**single byte load with absolute disp16**, no pointer materialization:

```
a0 02 00                ; mov al, [0x0002]   ← FIXUPP-relative to string base + 2
98                      ; cbw                ← widen to int for return
```

The opcode `a0 disp16` is the AL-form `mov al, moffs8` — a special
accumulator-only encoding that takes a 16-bit absolute address. The
`02 00` displacement is the FIXUPP offset that the linker resolves
to "string base + 2" once the string literal `"ABCDEF\0"` (`41 42 43
44 45 46 00`) is placed in `_DATA`.

So `"STR"[K]` for a constant `K`:
- The compiler treats the literal subscript as `*(literal + K)`.
- `literal + K` is itself a constant (linker-resolved address +
  parse-time `K`).
- The deref folds to the single `a0 disp16` byte-load form.

No `mov bx, offset str / mov al, [bx+K]` round-trip — BCC peepholes
directly to the moffs8 form.

## Octal escape sequences `\NNN` (fixture `2423`)

`"\003\012\077"` — three octal escapes in a single string:

```
data section:
  03 0a 3f 00   ; \003=3, \012=10, \077=63, terminator
```

So octal escapes work in BCC:
- `\003` → 3 (octal 003 = decimal 3)
- `\012` → 10 (octal 012 = decimal 10)
- `\077` → 63 (octal 077 = decimal 63)

Earlier docs noted "Octal escapes await fixtures" — this fixture
demonstrates the feature is implemented. The lexer's `decode_escape`
helper must recognize a backslash followed by 1-3 octal digits
(0-7) and consume greedily, same as `\x` but with octal digits.

A trailing non-octal character ends the escape; `\1234` would be
parsed as `\123` (= octal 123 = 83) followed by `'4'`.

Limit: 3 octal digits max per the C standard. `\0` is the special
single-zero case (NUL terminator), since it's `\` followed by a
single octal digit `0` followed by end-of-octal-digits.

## `char *names[] = { "Al", "Bo", "Cy" }` — pointer table + interleaved strings in _DATA

Fixture `2551-array-of-strptr-obj`:

```c
char *names[] = { "Al", "Bo", "Cy" };
int main(void) {
  return names[1][0];
}
```

`_DATA` layout (15 bytes):
```
06 00 09 00 0c 00     ; pointer table: 3 × 2B offsets (FIXUPPs into _DATA)
41 6c 00              ; "Al\0" at offset 6
42 6f 00              ; "Bo\0" at offset 9
43 79 00              ; "Cy\0" at offset 12
```

Main body:
```
55 8b ec                       prologue
8b 1e 02 00                    mov bx, [_names+2]    ; bx = names[1]
8a 07                          mov al, [bx]          ; *names[1]
98                             cbw                   ; → int
eb 00 5d c3                    epilogue
```

Findings:
- BCC lays out the **pointer table first**, then **packs the strings
  immediately after** in the same `_DATA` segment. The pointers
  themselves are FIXUPPs into `_DATA` at the string offsets.
- This is the canonical "array of string pointers" layout — no
  fragmentation. Same `_DATA` segment for all parts.
- `names[1][0]` requires **two memory loads**: first the pointer
  (`mov bx, moffs16`), then the byte (`mov al, [bx]`). No fold to
  a single moffs8 because the pointer indirection breaks the chain.
- Each string is null-terminated and packed back-to-back; no
  alignment padding between strings.


## Mixed globals + string literal — declaration order in `_DATA`

Fixture `2565-string-then-data-obj`:

```c
int n = 42;
char *s = "ZZ";
int main(void) {
  return n + s[0];
}
```

`_DATA` layout (7 bytes total):
```
2a 00       ; offset 0: _n = 42
04 00       ; offset 2: _s = &"ZZ" (= _DATA + 4, FIXUPP'd)
5a 5a 00    ; offset 4: "ZZ\0" literal
```

Findings:
- Globals are laid out in **declaration order** in `_DATA`. Each
  occupies its declared size: 2B for int, 2B for char* (near ptr).
- The string literal `"ZZ"` lands in the SAME `_DATA` segment,
  immediately after the regular globals. So mixed init goes in
  one contiguous segment.
- The pointer `_s` is FIXUPP'd to point to offset 4 (where the
  string lives) in `_DATA`. The FIXUPP target is the segment, with
  disp16=4 in the storage.
- This contrasts with `char s[] = "ZZ"` where s would BE the bytes
  (no pointer indirection). The `char *s` form uses a separate
  pointer slot + the literal.
- The expression `n + s[0]` uses the **push/pop pattern** again to
  juggle int and char (cbw-promoted) results through AX.


## Global `int v = -1` — two's complement in `_DATA`

Fixture `2599-global-int-neg-obj`:

`_DATA` bytes: `ff ff` (= -1 in 16-bit two's complement)

Findings:
- Negative integer literal initializers are stored as **two's
  complement bytes** in `_DATA`. No special runtime negation.
- The same shape as positive globals: 2 bytes, little-endian.
  Only difference: bit pattern.
- BCC's parser evaluates the `-1` literal at compile time and
  emits the resulting word.


## `char arr[N] = { 'h', 'i', 0, 'x', 'y', 0 }` — exact byte init

Fixture `2637-char-arr-explicit-nul-obj`:

```c
char msg[6] = { 'h', 'i', 0, 'x', 'y', 0 };
```

`_DATA` bytes (6 = exact):
```
68 69 00 78 79 00       ; h i \0 x y \0
```

Findings:
- An explicit char-init list emits each character as a separate
  byte in `_DATA`. Embedded `0` values are preserved (no NUL
  terminator interpretation — this isn't a string literal).
- Equivalent in bytes to a string literal `"hi\0xy"` with a
  trailing `\0` added, but the brace form is the only way to put
  embedded NULs at arbitrary positions cleanly.
- The init list size matches the array size exactly — no zero-fill
  needed because the source has 6 explicit values.
- Compare to `char buf[5] = "hi"` (`2561`) which zero-fills the
  remaining 3 bytes. The brace form has NO zero-fill if all slots
  are filled by the user.


## Global init with unary-minus over arithmetic — fully folded

Fixture `2650-global-init-neg-expr-obj`:

```c
int v = -(2 + 3 * 4);    /* = -14 */
```

`_DATA` bytes for `_v`: `f2 ff`  (= 0xFFF2 = -14)

Findings:
- BCC's constant evaluator handles **unary minus on a constant
  expression** at compile time. The fold proceeds:
  - `3 * 4` → 12
  - `2 + 12` → 14
  - `-(14)` → -14 → 0xFFF2 in two's complement
- Stored as 2-byte little-endian: `f2 ff`.
- Same pattern as `2547` (`int n = 2 + 3 * 4` = 14) but with the
  outer unary minus also folded.
- Confirms: any C90 constant expression in a static initializer is
  fully evaluated by the parser; runtime never executes any init
  code.

