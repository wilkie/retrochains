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

### `unsigned char` as param / pointee / return

Fixtures `464`–`466` extend uchar to function param, pointer
target, and return type. Two more latent gaps surfaced:

1. **Byte-store through a pointer register** wasn't supported in
   the assembler — `mov byte ptr [si], imm8` (`C6 04 ii`, 3
   bytes). Added as `Instr::MovSiPtrImm8`, paralleling the existing
   word-form `MovSiPtrImm` (`C7 04 ww ww`). Fixture 465 uses it
   for `*p = 200;` where `p: unsigned char *` in SI.
2. **Return value widening differs by signedness**: signed `char`
   return emits `cbw` after the AL load (fixture 156); unsigned
   `char` return emits *no widening* — the value lives in AL alone
   and the upper byte is left undefined. Fixture 466 pins the
   uchar shape: `mov al, byte ptr DGROUP:_g / jmp short epilogue
   / ret`, no `mov ah, 0`. `emit_return_value_load` now
   short-circuits the uchar-ident case to emit just the byte load.

### BSS layout — short bucket first

Fixture `465` (uchar array + int global) exposed that BCC's _BSS
member ordering is **not** pure alphabetical. The actual rule:
*short-named globals (mangled `_<name>` length < 3) in alphabetical
order first, then long-named globals in alphabetical order*. This
is the same length-bucket discriminant the publics emission uses,
applied to globals (and effectively the reverse of the publics
emission order, filtered to BSS members).

The earlier "alphabetical" reading happened to coincide on
fixtures 181/234/462 because their names all fell in the same
bucket. 465 has `_buf` (4 chars including underscore) in the long
bucket and `_g` (2 chars) in the short bucket, so the buckets
diverge: oracle emits `_g, _buf` (short → long) with no padding,
not the alphabetical `_buf, _g` that would require a 1-byte pad
to align `_g`.

### Publics emission — long bucket direction

The long bucket's sort direction depends on whether the source
has either a short-named global *or* a long-named **initialized**
global (one that lands in `_DATA`). Fixture `498`
(`char msg[16] = "hello"; int main { return 0; }`) forced the
latest refinement: even with no short global, a `_DATA`-resident
long global flips the order to forward. The current rule:

- If the long bucket contains a global **and** the source has at
  least one short-named global, **or** the source has at least one
  long-named global with an initializer (in `_DATA`): emit the
  long bucket in **forward** alphabetical order (globals,
  functions, helpers mixed together).
- Otherwise: emit in **reverse** alphabetical order.

Pinning fixtures:

- 095 (`_sum`, `_main`) — no short global, no DATA global →
  reverse → `_sum, _main`.
- 179 (`_add`, `_main`) — same → reverse → `_main, _add`.
- 260 (`_main`, `N_LXMUL@`) — short globals `_a, _b` present, but
  long bucket has no global → reverse → `_main, N_LXMUL@`.
- 465 (`_buf`, `_main`) — long global + short global `_g` →
  forward → `_buf, _main`.
- 491 (`_pts`, `_main`) — long global + short global `_g` →
  forward → `_main, _pts`.
- 494 (`_head`, `_main`) — long BSS global only, no short global,
  no DATA global → reverse → `_main, _head`.
- 498 (`_msg`, `_main`) — long DATA global (`msg[16] = "hello"`)
  → forward → `_main, _msg`.

Fixture `506` (`int helper(int); int main { return helper(7); }
int helper(int x) { ... }`) adds a third trigger: a function
prototype (`body: None` followed later by a matching definition)
flips the order to forward. Updated rule:

- If `(long has global AND short has global)` OR `long has an
  initialized DATA global` OR `there is any function prototype`,
  use **forward** alphabetical for the long bucket.
- Otherwise use **reverse**.

Pinning fixture: 506 → forward → `_helper, _main`.

The intuition is that BCC's internal iteration order is sensitive
to which symbol table buckets are non-empty: initialized-data
globals, short-named globals, and seen-twice symbols (prototype +
def) all leave a record that flips a "forward iteration" mode for
the long bucket. The exact mechanism remains unclear; further
fixtures may force more refinement.

## `signed` keyword

`signed`, `signed char`, `signed int`, `signed long [int]`,
`long signed [int]` are all accepted by `parse_type` and lower to
the corresponding signed types (`Type::Int`, `Type::Char`,
`Type::Long`). Codegen is identical to the unprefixed forms
since BCC's plain `char`/`int`/`long` are already signed. Fixtures
`467` (`signed char`) and `468` (`signed int`) round-trip to the
same bytes as the unprefixed equivalents — the keyword is purely
front-end.

## `enum <tag>` as a type

In addition to anonymous `enum { A, B, C };` (which only registers
the member constants), `enum <tag>` can now also be used as a type
name in declarations. Fixtures `470`–`472` exercise this as a
global type, local type, and function-parameter type respectively.

Codegen: `enum <tag>` lowers to `Type::Int` (BCC sizes enums as
int). No special storage, comparison, or widening — purely a
front-end alias.

Parser:
- The standalone `enum [<tag>] { … };` dispatcher in `parse_unit`
  now only fires when an opening `{` follows the (optional) tag.
  When the form is `enum <tag> <decl>` the dispatcher skips and the
  type-prefix path handles it.
- `parse_type` learned `enum [<tag>]` → `Type::Int` (the tag is
  consumed if present but we don't require it to be in any tag
  table — the enum members were registered at the definition site).
- The top-level type-probe gained an `enum [<tag>]` arm.
- `parse_stmt`'s declaration dispatch now accepts `KwEnum` (and
  `KwSigned`, completing the set started in batch 50) as a type
  start, so `enum color c;` works inside function bodies.

Explicit member values (`enum flag { OFF = 0, ON = 1, AUTO = 7 }`)
also flow through the same path — the body parser already accepted
`= <int-lit>` per-member, and the values fold into `IntLit` at use
sites (fixture `473`).

The body form `enum [<tag>] { … } <decl>` (combined definition +
declaration) works too. Fixture `474`'s
`enum { A = 1, B = 2, C = 3 } x;` declares both the constants and
a local `x`. Implementation factored `parse_enum_body` out of
`parse_enum_decl` so `parse_type`'s enum branch can reuse it; the
caller consumes through `}`, then the surrounding declare path
sees the declarator.

## `const` / `volatile` / `register` qualifiers

`const`, `volatile`, and `register` are accepted as discardable
qualifiers — BCC keeps the storage layout identical to the
unqualified form. Fixtures `475` (const global), `476` (volatile
global), and `477` (register local) all round-trip to bytes that
match the equivalent unqualified declaration.

Implementation: a single `while` loop at the top of `parse_type`
consumes any combination of these three keywords, and a parallel
consumer runs at the start of the top-level type-probe. All three
are also accepted as type-starts in `parse_stmt`'s declaration
dispatch. No AST node — the qualifiers are just dropped.

Note: BCC's actual `register` keyword is a *hint* that forces
enregistration even below the natural use-count threshold (the
oracle for `register int x; x = 5; g = x;` enregisters `x` into
SI even though `x` only has 2 uses, below the int-enregistration
threshold of 3). We don't yet honor that hint — fixture `477` uses
`x` three times so it enregisters naturally; if a real fixture
requires register-hint enregistration the allocator will need a
new bias channel.

### Register-resident int → global store

While unblocking fixture `477` a separate gap turned up: `g = x`
where `x` is a register-resident int local was emitted as
`mov ax, si / mov word ptr [_g], ax` (5 bytes, AX round-trip).
BCC emits the direct `mov word ptr [_g], si` (`89 36 disp16`, 4
bytes). `emit_assign_global` now special-cases register-resident
int RHS to use the register-source-to-global form via the existing
`MovGroupSymReg16` instruction.

## Multi-declarator globals

`int a, b, c;` at file scope now works (fixture `478`).
`parse_global` switched from returning a single `Global` to
returning `Vec<Global>` — the comma loop mirrors the existing
local-decl multi-declarator handling. Each tail declarator
re-applies its own pointer stars and array suffix to a fresh
copy of the base type, exactly like `int *a, b;` does for
locals (fixture `174`). Per-declarator initializers
(`int a = 1, b = 2, c = 3;` — fixture `479`) and mixed forms
(`int *p, y;` — fixture `480`, `extern int e1, e2;` —
fixture `481`) all flow through the same loop.

### `&<global>` at runtime

Fixture `480` exposed two gaps:

1. **`&<global>` in runtime expression position** —
   `emit_address_of` only handled stack-resident locals. Now also
   emits `mov ax, offset DGROUP:_<g>` for globals.
2. **`<ptr-global> = &<global>;` direct immediate-store form** —
   `mov word ptr DGROUP:_p, offset DGROUP:_x` (`C7 06 <p-disp>
   <x-imm>`, 6 bytes with TWO FIXUPPs — one on each disp16). Added
   as `Instr::MovGroupSymOffsetGroupSym`; encoder reuses
   `emit_group_sym_lea` for the dst-disp FIXUPP and a new
   `emit_group_sym_imm16` helper for the src-imm FIXUPP (same
   shape, minus the opcode prefix). Without this special case the
   codegen bounces through AX (`mov ax, offset _x / mov [_p], ax`,
   5 bytes — shorter, but not what BCC emits).

### Data-extern emit order

The data-extern emit loop walks `unit.globals.iter().rev()`.
Single-extern fixtures never exposed the rule; fixture `481`
(`extern int e1, e2;`) pinned reverse-declaration order:
`extrn _e2:word` first, then `extrn _e1:word`.

### `&<arr>[K]` for global arrays

Fixture `483` exercises `p = &a[2];` where `a` is a global array.
The runtime form is parallel to `&<global>`: emit the symbol+offset
as an immediate. Two paths updated:

- `emit_expr_to_ax` for `AddressOfArrayElem`: emit
  `mov ax, offset DGROUP:_<array>[+<byte_offset>]` for global
  arrays (stack-resident locals would need LEA; no fixture yet).
- `emit_assign_global` for `<ptr-global> = &<arr>[K];`: emit
  `mov word ptr DGROUP:_<p>, offset DGROUP:_<array>[+<byte_offset>]`
  — uses the same two-FIXUPP `MovGroupSymOffsetGroupSym`
  instruction added in batch 54 (the parser's
  `parse_offset_group_symbol` already handles the `+N` suffix on
  the source symbol).

### `&<global>` as a call argument

Fixture `482` (`f(&g);`) passes through the existing call-arg
path: `emit_call` calls `emit_expr_to_ax` on the argument, which
hits the new `emit_address_of` global branch and emits
`mov ax, offset DGROUP:_<g>` before the `push ax`. No new case
needed.

### Extern array decay in expressions

Fixture `484` (`extern int a[5]; return a[0];`) passes through the
existing array-decay path. The codegen treats `a[0]` as a regular
global-array index — `mov ax, word ptr DGROUP:_a` (extern resolves
to base-of-array, offset 0). No new code; the existing
`emit_array_index_to_ax` path handles extern arrays the same as
defined ones since the address-lowering goes through the same
`<group>:_<name>+<offset>` template.

### `&<struct>.<field>` for globals

Fixture `485` exercises `p = &s.y;` where `s` is a global struct.
The parser now consumes any `.<field>[.<field>]*` chain after
`&<ident>`, threading the type table to compute the cumulative
field byte_offset and producing
`ExprKind::AddressOfArrayElem { array, byte_offset }` — the same
node shape `&<arr>[K]` produces. The existing
`MovGroupSymOffsetGroupSym` immediate-store form then emits
`mov word ptr DGROUP:_<p>, offset DGROUP:_<s>+<field-offset>`.

### `&<local-arr>[K]` — LEA from bp-offset

Fixture `486` exposed the missing local-array case in the
`AddressOfArrayElem` codegen. For stack-resident local arrays we
now emit `lea ax, word ptr [bp+off+K]` where `off` is the array's
bp-relative offset and `K` is the byte offset of the indexed
element. Encoded as `8D 46 disp8` for small offsets. The parser's
`&<ident>[K]` path was previously restricted to *global* arrays —
extended to also accept stack-resident locals by consulting
`function_locals` when `global_types` doesn't have the name.

### `typedef <type> *<name>;` — pointer typedef

Fixture `487` (`typedef int *INTP; INTP p; p = &g;`) needed the
typedef parser to consume pointer stars between the base type and
the name. Added a `while … Star` loop in `parse_typedef` matching
the existing shape in `parse_declare` and `parse_global`. The
typedef table then stores the wrapped pointer type, and uses of
`INTP` resolve to `Pointer(Int)`.

### `typedef <type> <name>[N];` — array typedef

Fixture `488` (`typedef int IARR[3]; IARR a;`) needed the
typedef parser to consume an array-suffix tail. Added the same
`while … LBracket` loop that `parse_declare` already uses,
wrapping innermost-first so a multi-dim
`typedef int M[2][3];` would yield `Array{2, Array{3, Int}}`.

### `typedef struct { … } <name>;` and typedef-of-typedef

Fixtures `489` (`typedef struct { int x; int y; } Point;`) and
`490` (`typedef int INT; typedef INT *INTP;`) both passed first
try. `parse_type` already handles inline `struct { … }` literals
and resolves a typedef-name as the referent type when it appears
where a type is expected — so a typedef whose base is another
typedef just flows through. The pointer-of-typedef in 490
exercises the right composition order at the typedef level.

### Multi-dim global arrays and nested initializers

Fixture `492` (`int a[2][3] = { {1,2,3}, {4,5,6} };`) needed two
parser extensions:

1. `parse_global` now loops the array suffix (`while LBracket`)
   instead of accepting at most one `[N]`. First suffix may still
   be `[]` for length-inference from the initializer.
2. `parse_initializer` now recurses for nested braces. Multi-dim
   array inits embed `InitList` inside `InitList`.

### Local declarations: aggregate initializer

`finish_declare` (the common tail for local Declare/static-local
hoisting) now calls `parse_initializer` instead of `parse_expr`
for the `= <init>` slot, so static locals with braced
initializers (`static int a[3] = {10, 20, 30};` — fixture `493`)
work. Scalar locals are unaffected: `parse_initializer` falls
through to `parse_expr` when no `{` is seen.

### Self-referential struct, forward struct decl, struct array field

Fixture `494` (`struct node { int value; struct node *next; };`)
needed `parse_record_type` to pre-register the tag as an empty
placeholder *before* parsing fields so that
`struct node *next` can resolve to a pointer to the in-progress
struct. The placeholder is replaced with the complete type once
all fields are parsed. The codegen for `head.next = &head;` was
unsupported (non-constant rhs panic in `emit_member_assign`);
now special-cases `&<global>` rhs to use the
`MovGroupSymOffsetGroupSym` two-FIXUPP immediate-store form.

Fixture `495` (`struct point; struct point *p; struct point
{...};`) needed a bare forward-declaration form
(`struct <tag>;`) in `parse_unit` to register the tag as an
opaque placeholder so subsequent `struct <tag> *p;` resolves.
The eventual full definition replaces the placeholder via the
same `self.structs.insert` path.

Fixture `496` (`int *f(void) { return &g; }`) needed
`parse_function` to consume pointer stars between the return
type and the function name — same shape as `parse_declare` /
`parse_global` already had. Returning `int *` from a function
flows through the existing pointer-typed return path; the
top-level type-probe already accepted the stars.

### Struct array fields, fnptr fields, struct-array-element write

Fixture `497` resolved the struct-field-of-array element write
that was deferred from an earlier batch: `struct buffer { int
len; int data[4]; }; b.data[2] = 42;`. Parser now adds `[expr]`
to `parse_atom`'s postfix loop so `b.data[2]` parses as
`ArrayIndex { array: Member { base: Ident(b), field: data },
index: 2 }`. The lvalue walker in `parse_expr_or_lvalue_assign`
detects this `ArrayIndex(Member(Ident, field), ...)` shape and
lowers it to a new `StmtKind::MemberArrayAssign { base, field,
indices, value }`. Codegen folds field-offset + Σ(idx·stride)
into a single byte displacement off the struct base and emits
one `mov word ptr DGROUP:_b+N, K` (or bp-relative for locals).
For the rvalue side (`g = b.data[2]`), `emit_array_index_to_ax`
has a similar fast-path that recognizes the same shape and emits
a single `mov ax, word ptr DGROUP:_b+N`.

The struct-fnptr field (`int (*fn)(int);` in a struct) similarly
needs new declarator support and remains deferred.

### Char array initialized to a shorter string

Fixture `498` (`char msg[16] = "hello";`) — when the declared
array length exceeds `bytes.len() + 1` (the bytes plus the NUL
terminator), the remaining slots are zero-filled out to the
declared length. `emit_global_init`'s string-literal-into-char-
array path now emits `db <byte>` lines for each character, a
trailing `db 0`, then additional `db 0` lines until the declared
length is reached. The LEDATA payload in the resulting OBJ
matches BCC byte-for-byte, including the trailing zero pad.

### `static` function definitions

Fixture `499` (`static int helper(int x) { return x + 1; } int
main(void) { return helper(41); }`) — a function with `static`
storage class is emitted in `_TEXT` like any other function
*but* never gets a `public _helper` declaration. `parse_unit` now
accepts `static` (and only `static`, not `extern`) before a
function definition, recording it on `Function::is_static`.
`emit_s.rs`'s publics loop skips static functions when building
the long/short bucket. Codegen for calls is unchanged: the call
site emits `call near ptr _helper` because TASM resolves
`_helper` within the same `_TEXT` segment without needing an
`extrn`. The `_helper` PUBDEF simply isn't emitted in the
resulting OBJ, matching BCC's output.

## Comma operator

`<expr>, <expr>` at expression level is a comma operator —
distinct from the comma as argument-list / init-list separator.
C grammar only permits it in a *comma-expression* context:
parenthesized expressions and the top of expression statements.
Implementation only handles the parenthesized form for now
(`g = (a = 1, b = 2, a + b);` — fixture `469`).

Each element inside the parens is parsed via
`parse_for_clause_expr`, which already recognizes
`<ident> = <expr>` as `ExprKind::AssignExpr`. The comma-separated
elements chain left-associatively into nested
`ExprKind::Comma { left, right }` nodes.

Codegen: the left side is discarded (side effects only) and the
right side produces the value. In `emit_expr_to_ax`, the comma
maps to `emit_expr_discard(left)` then `emit_expr_to_ax(right)`.
`emit_expr_discard` recursively handles Comma so nested chains
like `(a = 1, b = 2, a + b)` discard all but the rightmost
element correctly.

## Chained assignment

Fixture `500` (`int a, b, c; a = b = c = 5;`) — C's `=` is
right-associative and yields a value, so `a = b = c = 5` parses
as `a = (b = (c = 5))`. The statement-level dispatch for
`<ident> = …` now uses `parse_for_clause_expr` (rather than
`parse_expr`) for the RHS, so the RHS can itself be another
`AssignExpr`. `parse_for_clause_expr` was made recursive on its
RHS to support the chain.

Codegen for `AssignExpr` in value position lives in
`emit_expr_to_ax`: it recursively evaluates the inner value into
AX, then emits one `mov word ptr <target>, ax` for the
side-effect store. AX still holds the assigned value so the
outer assignment reuses it. The resulting sequence for `a = b =
c = 5;` is `mov ax, 5; mov [_c], ax; mov [_b], ax; mov [_a],
ax` — one literal load and three stores, exactly what BCC emits.

## `*p++ = v;` — store via pointer with postfix increment

Fixture `501` (`*p++ = 7; *p++ = 8;` filling a local `int[3]`)
exercises the postfix-increment pattern as an lvalue.
`emit_deref_assign` now special-cases `DerefAssign { target:
Update{p, Inc, Post}, value }`: it emits the store first (using
the pre-increment register value as the address) and then
advances the register by `sizeof(*p)` via `inc <reg>` per byte
of stride. For `int *p` in SI, the result is `mov word ptr [si],
v; inc si; inc si`. This relies on the pointer being
register-resident — no fixture exercises stack-resident `p++`
in this position yet.

## Partial array initializer

Fixture `502` (`int a[5] = {1, 2};`) — when an aggregate
initializer has fewer items than the declared array length, the
missing slots are zero-filled out to the full byte size.
`emit_global_init`'s `InitList` arm now emits `db 0` lines for
`(len - items.len()) * elem.size_bytes()` after the explicit
items. This mirrors the trailing-zero pad behavior we added for
fixed-length char-array string initializers in fixture 498.

## Pointer compared to integer constant

Fixture `504` (`int *g; if (g == 0) return 1;`) — comparing a
global pointer to a constant must use the memory-direct
`cmp word ptr DGROUP:_g, 0` form, not the load-then-compare
sequence. The `emit_compare` int-global shortcut now triggers
for any global whose type has a pointee, in addition to plain
`int`/`unsigned`. The same `83 3E disp16 ii` (imm8sx) encoding
applies; no new IR was needed.

## Forward function declaration

Fixture `506` (`int helper(int); int main(void) { return
helper(7); } int helper(int x) { return x + 1; }`) — a
function prototype followed later by the matching definition.
Two parser changes:

- `parse_param_list` now allows anonymous parameters (`int
  helper(int)` with no parameter name). When the token after a
  type is `,` or `)`, the parser synthesizes
  `__anon_param_<n>` as the name. Codegen never references these
  for a prototype-only Function (the body is `None`), so the
  synthesized name is purely a slot-filler.
- The publics ordering rule gains a third trigger: presence of
  any function prototype (`body: None`) flips the long-bucket
  emission to forward alphabetical. 506's expected output has
  `_helper, _main` (alphabetical), where 095/179 with no
  prototype use reverse.

The prototype itself is emitted as a no-op (no asm, no PUBDEF).
Only the actual definition contributes a `public _helper` line.

## `for(;;)` — empty condition

Fixture `507` (`int main(void) { int i; i = 0; for(;;) { if
(i > 5) break; i = i + 1; } return i; }`) — when the for's cond
is absent the trampoline `jmp short <check>` at loop entry is
elided. BCC layouts the body directly at the loop label and
falls through into the test/body without first jumping past the
nothing-to-check guard. `emit_for` now skips the trampoline
when `cond.is_none()`.

## Adjacent string literal concatenation

Fixture `508` (`char *s = "Hello, " "World";`) — C90 specifies
that adjacent string-literal tokens concatenate at translation
time. `parse_primary`'s StringLit arm now peeks for further
StringLit tokens, appending their bytes (and extending the
span) before returning a single `ExprKind::StringLit`. The
combined literal flows through the existing pool path, so
nothing downstream needed to change.

## Variable-indexed global int-array store

Fixture `510` (`int a[5]; for (i = 0; i < 5; i = i + 1) a[i] =
i;`) — when the index of a global word-element array isn't a
constant, codegen now loads it into BX (directly from a stack or
register local), shifts left by 1 for the word stride, and emits
`mov word ptr DGROUP:_a[bx], <src>` where `<src>` comes from
`resolve_operand_source(value)`. A new IR variant
`MovGroupSymBxDispReg16` encodes the `89 mod=10 reg r/m=111`
form (e.g. `89 b7 lo hi` for SI) — the immediate-source sibling
`MovGroupSymBxDispImm` was already in place for long-array
writes (fixture 305).

## `sizeof` of a string literal

Fixture `511` (`return sizeof("hi");`) — extended
`expr_static_size` to handle `ExprKind::StringLit`. The result is
`bytes.len() + 1` to include the NUL terminator. The expression
folds to an `IntLit` at parse time, just like `sizeof(<type>)`.

## Global `g++` / `g--` statement

Fixture `512` (`int g; g++; g++; return g;`) — `emit_update_in_
place` previously handled only long globals via the `add/adc 1`
pair. Plain int (and char) globals now emit the single
memory-direct `inc word ptr DGROUP:_g` (or `dec` / `byte ptr`
for char). Two new IR variants — `IncGroupSym` and `DecGroupSym`
— encode the `FF 06 lo hi` and `FF 0E lo hi` forms (Grp5 /0 INC
and /1 DEC, ModR/M r/m=110 → `[disp16]`).

## Assignment expression in `if` condition

Fixture `513` (`if ((x = 5)) return x;`) — when the condition
is `AssignExpr`, BCC evaluates the assignment (storing the value
and leaving it in AX), then emits `or ax, ax` to set the flags
for the conditional branch. `emit_zero_test` now special-cases
`ExprKind::AssignExpr`: route through `emit_expr_to_ax` (the
chain-assignment path landed in batch 61) and append the `or
ax, ax` post-test.

## Compound assigns on int global

Fixture `517` (`int g; g &= 15;`) and `519` (`g += 5;`) —
`emit_compound_assign` had no int-global path, so any `g <op>=
K` panicked on the local-only lookup. The fix added two memory-
direct emit paths against int (and uint) globals when RHS folds
to a constant:

- `BinOp::BitAnd / BitOr / BitXor` → `<and|or|xor> word ptr
  DGROUP:_g, K`. BCC always uses the imm16 form here (no
  imm8sx peephole for bitwise ops). Routes through the existing
  `AndGroupSymImm16` / `OrGroupSymImm16` / `XorGroupSymImm16`
  IR variants (originally introduced for long globals).
- `BinOp::Add / Sub` → `add|sub word ptr DGROUP:_g, K`. TASM
  picks the imm8sx form when K fits a signed byte (so `g += 5`
  encodes as `83 06 lo hi 05`, the 5-byte form) and the imm16
  form otherwise (`81 06 lo hi imm_lo imm_hi`).

## Empty statement

Fixture `522` (`for (i = 0; i < 100; i = i + 1) ;`) and `523`
(`while (g) ;`) — C90's null statement `;` was a parse error
because `parse_stmt` had no arm for bare semicolons. Added
`StmtKind::Empty` and an entry in `parse_stmt` that consumes the
single `;`. Codegen for `Empty` produces nothing (the loop's
back-edge / condition handling still runs because they're owned
by the surrounding `emit_for` / `emit_while`, not the body).
Adding the new variant required no-op arms in every match on
`StmtKind` (locals.rs use-counts, plan.rs label planner, emit_
s.rs call walker, codegen/mod.rs emit_stmt) — same pattern as
when `Goto`/`Label` were introduced.

## Char local compared to constant

Fixture `524` (`char c; c = 'A'; if (c == 'B') ...`) — the
stack-local compare path in `emit_compare` always emitted `cmp
word ptr [bp-N], K`, but for a char local BCC uses the byte
form `cmp byte ptr [bp-N], K` (encoded `80 7E disp8 imm8`). The
fix: check `ty.is_char_like()` on the named local and emit the
byte form. A new IR variant `CmpByteBpRelImm8` encodes it.
Parser handles `cmp byte ptr [bp+N], imm8` via the existing
`parse_byte_bp_relative` helper.

## Negative case label

Fixture `525` (`switch (x) { case -1: return 10; ... }`) —
`parse_switch`'s case head only accepted `IntLit` directly. It
now allows an optional leading `Minus` token and negates the
literal via `wrapping_neg` so the case value stays a u32 with
the same wrap-around semantics that `try_const_eval` produces
for `-1`. Codegen needed no change — switch comparison already
handles arbitrary u32 case values.

While integrating this, found a bug in `emit_assign_local`'s
stack-int immediate store: `try_const_eval` returns u32, so
`x = -1` was emitting `mov word ptr [bp-2], 4294967295`. Now
the path masks to `v & 0xFFFF` before formatting (matching the
already-correct char form). All prior fixtures still hit the
same byte output because their constants fit in 16 bits without
sign-extension; only the negative-literal path tripped this.

## Switch on a char scrutinee

Fixture `527` (`char c; c = 'B'; switch (c) { case 'A': ... }`)
— `emit_switch_chained`'s scrutinee load asserted Int locals
only. Char locals now load via `mov al, byte ptr [bp-N]; cbw`
(or `mov ah, 0` for uchar via `emit_widen_al`), promoting the
byte to AX before the chained `cmp ax, K / je` sequence. Case
values are 16-bit constants regardless of scrutinee type — BCC
uses `cmp ax, 0x42` even though the live value only occupies
AL.

## Char compound assign on a byte-register local

Fixture `529` (`char c; c = 'A'; c += 2;`) — `emit_compound_
assign` asserted out for byte-register dests. Added an AL-
round-trip path for `Add/Sub/BitAnd/BitOr/BitXor` with a
constant RHS:

```
mov al, <reg>
<op> al, K
mov <reg>, al
```

This required five new AL-specific tasm IR variants (`AddAlImm8`,
`SubAlImm8`, `AndAlImm8`, `OrAlImm8`, `XorAlImm8`) for the
2-byte accumulator forms (`04 ii`, `2C ii`, `24 ii`, `0C ii`,
`34 ii`). BCC always picks the AL accumulator form when AL is
the destination; the generic `80 C0+rc ii` 3-byte encoding
appears only for non-AL byte registers, which we haven't
fixtured yet.

## `y = ++x;` direct-stack-store peephole

Fixture `530` (`int x; int y; x = 5; y = ++x;`) — BCC fuses the
pre-increment with the subsequent stack store, skipping the AX
round-trip when the source is a register-resident local and the
dest is a stack slot. `emit_assign_local`'s Stack branch now
detects `Update { target, op, position: Pre }` against a
non-byte reg-local and emits `<inc|dec> <reg>; mov word ptr
[bp-N], <reg>` directly (4 bytes instead of 5 — saves the
`mov ax, <reg>` step). Post-update keeps the round-trip because
the expression value is the *old* register contents.

## Local shadowing a global

Fixture `532` (`int a; int main() { int a; a = 7; return a; }`)
— C90 scoping says the local `a` hides the global `a` inside
the function body. Codegen's ident resolution was global-first,
so writes went to the global slot and reads via `a1 lo hi`
(`mov ax, [_a]`). Both paths (`StmtKind::Assign` dispatch and
`ExprKind::Ident` in `emit_expr_to_ax`) now check `locals.has`
before the global table, falling through to the local lookup
when the name is in scope. Other codegen sites that check
`globals.contains` only matter when the local doesn't exist, so
no further changes were needed for this fixture — but the pattern
will likely need extending if more shadowing cases appear.

## NULL pointer init

Fixture `533` (`int *g = 0;`) — global pointer initialized to a
null integer constant. The existing scalar-global-init path
handles this directly: codegen emits `dw 0` for the 2-byte
slot. No special-case needed because pointer types have the same
2-byte width as int.

## `return x++;`

Fixture `534` (`int x; x = 5; return x++;`) — worked on the
first try. The existing `emit_update_to_ax` already emits the
post-increment sequence `mov ax, <reg>; inc <reg>` and the
return path loads AX, which is exactly what BCC produces.

## Char shift compound

Fixture `535` (`char c; c = 4; c <<= 2;`) — BCC unrolls a char
compound shift by a small constant K into K single-bit shifts
directly on the byte register (`shl dl, 1; shl dl, 1`) rather
than the AL round-trip used for add/sub/bitwise (fixture 529).
The 8086 has no `r/m8, imm8` shift, only `r/m8, 1` and `r/m8,
cl`, so unrolling beats the 3-byte CL setup for small K.
`emit_compound_assign`'s byte-register path now handles
`BinOp::Shl/Shr` by emitting K `<shl|sar|shr> <reg>, 1`
instructions. Three new tasm IR variants (`ShlReg8One`,
`SarReg8One`, `ShrReg8One`) encode `D0 /4|/7|/5 r/m=<reg>` for
the byte form (sibling of `ShlReg16One`'s 16-bit form). Signed
char's `>>=` lowers to `sar` (sign-fill); uchar would lower to
`shr` (zero-fill — not yet fixtured at the byte width).

### Publics-ordering rule — still partial

While probing this batch, fixture `int echo(char c) { return c; }
int main { return echo('Z'); }` (originally proposed as 535)
revealed that the long-bucket forward/reverse rule has another
hidden dimension I can't yet characterize. Probing 0..10
parameter counts and different helper names shows BCC flips
between forward and reverse seemingly based on the helper's
name (`add` reverse, `helper` forward, `abc` forward, `addy`
reverse) regardless of param count. This suggests an internal
hash-bucket discriminator inside BCC's symbol table that we
can't replicate without more reversing work. The original 535
probe was replaced with a single-function fixture to sidestep
the issue.

## `if (!x)` logical-not condition

Fixture `536` (`int g; if (!g) return 1;`) — `!x` in a
condition context lowers to the same flag-setting test as
`x`, but the conditional jump's polarity flips. `emit_cond_
test` now special-cases `Unary { op: Not, operand }` by
recursing on `operand` and swapping the returned `(true_mnem,
false_mnem)` tuple. Nested `!!x` collapses correctly through
the recursion. The actual asm output is exactly what the
unnegated test produces — only the JE/JNE pairing on the
caller side differs.

## Int local compound shift

Fixture `537` (`int x; x = 1; x <<= 4;`) — register-resident
int local compound shift. `emit_compound_assign`'s register-
local branch now handles `BinOp::Shl`/`Shr` by emitting `mov
cl, K; <shl|sar|shr> <reg>, cl`. Three new tasm IR variants
(`ShlReg16Cl`, `SarReg16Cl`, `ShrReg16Cl`) generalize the
existing `ShlAxCl`/`SarAxCl`/`ShrAxCl` to any 16-bit register.
Signed `>>=` lowers to `sar` (sign-fill); unsigned `>>=` to
`shr`. BCC always uses the CL form here even for K=1 — no
unrolled `<reg>,1` peephole at this slot, unlike the byte-
register char path (fixture 535).

## Int global compound shift

Fixture `539` (`int g; g = 80; g >>= 2;`) — int (or uint) global
compound shift by a small constant K unrolls into K memory-
direct shift-by-1 instructions, just like the byte-register
char path (fixture 535) but with a `<group>:<sym>` memory
operand. Three new tasm IR variants (`ShlGroupSymOne`,
`SarGroupSymOne`, `ShrGroupSymOne`) encode `D1 26 | 3E | 2E lo
hi`. The codegen path picks `shl` for `<<=`, `sar` for signed
`>>=`, and `shr` for unsigned `>>=`. The unrolled emit slots in
before the existing add/sub and bitwise int-global compound
paths.

## Pointer compound add/sub — stride scaling

Fixture `542` (`int *p; p = a; p += 2;`) — pointer arithmetic in
compound form scales the RHS by `sizeof(pointee)`. `emit_
compound_assign`'s register-local Add/Sub path now multiplies
the const RHS by the pointee's `size_bytes()` before emitting
`add <reg>, scaled`. For `int *p` (stride 2), `p += 2` lowers
to `add si, 4`. The K==1 → `inc` peephole is now checked against
the *scaled* value, so it only fires when the actual byte
delta is 1 (i.e. char pointer with K==1).

## Switch on a non-ident scrutinee

Fixture `544` (`switch (x + 1) { case 1: ... }`) — when the
scrutinee isn't a bare ident, `emit_switch_chained` now routes
through `emit_expr_to_ax` and lets the result land in AX
directly. The chained-cmp+je sequence after the load is
unchanged. Ident scrutinees still hit the bespoke
char-widen/global-load shortcuts.

## `c = a % b;` — store DX directly

Fixture `546` (`int a, b, c; ... c = a % b;`) — after `idiv`,
the remainder lives in DX. The generic arith-to-AX path tacks
on a `mov ax, dx` so callers can find the result in AX, but
when the destination is a memory slot we can `mov [c], dx`
directly and save 2 bytes. `emit_assign_local`'s stack-int
branch now special-cases `BinOp::Mod` to emit `cwd; idiv <b>;
mov [bp-N], dx` via a small helper `emit_arith_setup_for_mod`.

## `++a[K]` peephole

Fixture `547` (`int a[3]; ... ++a[1];`) — `emit_array_compound_
assign` now folds K=1 add/sub into a single `inc|dec <width>
ptr [bp-N]` instruction (1 byte saved vs. `add mem, 1`). A new
tasm IR variant `IncBpRel` / `DecBpRel` encodes `FF 46|4E dd`
(Grp5 /0 or /1 with mod=01 r/m=110 → `[bp]+disp8`). The same
peephole was already in place for register-resident bare-ident
locals; this extends it to memory-direct stack array elements.

## Free passes (no code changes needed)

Three more probes hit existing paths byte-exactly:

- `548` — int local compound mul `x *= 3;` — already routed
  through the imul-via-AX skeleton.
- `549` — `if (x == g)` (int local vs int global) — the generic
  `emit_compare` Ident-load + memory-source path handles the
  asymmetric operand types.
- `550` — global int initialized to a folded constant expression
  `int g = 2 + 3 * 4;` — `try_const_eval` already folds nested
  BinOps at parse time, so the slot emits `dw 14` directly.

## `void` as a return type

Fixture `552` (`static void set(int *p) { *p = 99; }`) — parser
now accepts `void` as a return type. There's no dedicated
`Type::Void`; codegen treats functions with no `return <expr>`
statements identically regardless of declared return type, so
`Type::Int` serves as a placeholder. `parse_type` matches
`KwVoid` and the top-level type-probe in `parse_unit` includes
it.

While probing this, the publics-ordering rule revealed another
dimension: `void f(int *p)` + `int main(void)` (no statics)
emits `_main, _set` (forward), not the `_set, _main` reverse
that `int f(int *p)` would produce. Tested with many helper
names and the result depends on the helper's name in some hash-
bucket way we still can't characterize. Worked around by making
the helper `static` (which skips the PUBDEF emission entirely
and sidesteps the ordering question).

## `cmp ax, 0` → `or ax, ax` peephole

Fixture `555` (`while ((c = g) > 0) ...`) — when the right
operand of a compare folds to 0 and the left has just been
loaded into AX, BCC emits the 2-byte `or ax, ax` instead of the
3-byte `cmp ax, 0`. Both set ZF/SF identically so the
subsequent conditional jump works the same. Added at the tail
of `emit_compare` (after all global/local fast paths).

## Char compound: bitwise direct, arith via AL

Fixture `556` (`char c; c &= 31;` with c in DL) revealed that
BCC takes a different path for char compound based on the op
family:

- **Add/Sub**: route through AL — `mov al, <reg>; <add|sub> al,
  K; mov <reg>, al`. AL has 2-byte accumulator forms (`04/2C
  ii`) which beat the generic 3-byte form, so the round-trip
  pays off after `inc/dec` peepholes are applied. K=1 now
  collapses to `inc al`/`dec al` (fixture 553's int sibling
  pattern, here in 8-bit form).
- **And/Or/Xor**: emit `<and|or|xor> <reg>, K` directly with no
  AL detour (3 bytes total). Bitwise ops don't get the AL-
  accumulator advantage at K width 8, so the direct form wins.

Three new tasm IR variants — `AndReg8Imm8`, `OrReg8Imm8`,
`XorReg8Imm8` — encode `80 /4|/1|/6 mod=11 r/m=<reg> imm8`.
The AL-specific 2-byte forms (`AndAlImm8` etc.) stay for AL.

## `continue;` inside a for-loop — separate slot

Fixture `558` (`for (i = 0; i < 5; i = i + 1) { if (i == 2)
continue; s = s + i; }`) — the label planner reserved
`continue_target_slot` only when the body had *no* nested
labels (the "filler-slot" case for fixtures like 061). When the
body had nested labels (the `if` in 558 reserves two), the
planner re-used the next slot as both the continue-target *and*
the check-slot, so the emitter dropped two identical
`@1@N:` lines and `continue;` jumped to whichever the assembler
resolved first.

The fix: planner now runs a `body_has_continue` probe alongside
the body planning. When continue is present, it reserves a
distinct continue-target slot regardless of nesting. When
continue is absent and the body added no labels, it keeps the
historical filler reservation so the downstream label numbers
match the existing for-loop fixtures byte-for-byte. The
`body_has_continue` helper is duplicated into `plan.rs` (it
already lives in `codegen/mod.rs`); they walk the same Stmt
shape and need to agree.

## Global array decay → global pointer

Fixture `561` (`int a[3]; int *p; p = a;`) — array-to-pointer
decay at the assign site between two globals. `emit_assign_
global` now special-cases `Ident(src)` where `src` is a global
array, emitting the same `mov word ptr [_p], offset _a` form
(two-FIXUPP `MovGroupSymOffsetGroupSym`) we already used for
`p = &g;`. Without this, codegen mistakenly loaded the first
word at `_a` as if it were a value.

## Pointer global `++p` — stride

Same fixture: `++p` on a global pointer scales by
`sizeof(pointee)` instead of using `inc word ptr [_p]`. `emit_
update_in_place` now checks for pointer globals: if the stride
is ≠ 1 it emits `add|sub word ptr [_p], <stride>`. Char-pointer
globals (stride 1) still use the `inc/dec` peephole.

## Char return + caller-side widen

Fixture `562` (`char get(void) { return 'Z'; } int main { int
x = get(); }`):

- **Callee**: `emit_return_value_load` now detects char return
  type with a constant value and emits `mov al, K` (2 bytes)
  instead of `mov ax, K` (3 bytes). AH is undefined per BCC's
  char-return ABI.
- **Caller**: `ExprKind::Call` in `emit_expr_to_ax` consults
  `signatures.ret_ty_of(name)` and emits `cbw` (signed char) or
  `mov ah, 0` (uchar) after the call, widening AL into AX
  before downstream consumers (assignment, arithmetic) read
  the full int.

The two halves compose: the call site doesn't need to know how
the callee left AL — `signatures` provides the return type and
the widen step always fires.

## Compare with negative literal

Fixture `563` (`int x; if (x < -5) ...`) — two layered fixes:

- `emit_compare`'s stack-local and global-const paths masked
  the const to u32 width when formatting (`{rhs}`), so `-5`
  ended up as `4294967291` in the asm text. Both now mask to
  `& 0xFFFF` before emitting.
- TASM's `parse_imm8_signed` rejected u16 values in the upper
  half (32768..65535) even when they reinterpret as a fitting
  i8. The helper now reinterprets such values as `as i16` and
  retries the i8 fit, so `cmp word ptr [bp-2], 65531` correctly
  picks the imm8sx form (`83 7E dd FB`) BCC emits for `cmp ...,
  -5`. The wide-immediate sibling `CmpBpRelImm16` (`81 7E dd lo
  hi`) was added for true imm16 constants that don't fit i8sx.

## Pointer compound subtract

Fixture `564` (`int *p; p = a; p += 4; p -= 2;`) — `sub <reg16>,
imm` had no parser/encoder route in tasm. Added `SubReg16Imm8Sx`
(`83 E(reg) ii`, 3 bytes) and `SubReg16Imm16` (`81 E(reg) lo
hi`, 4 bytes). The codegen pointer-stride scaling from fixture
542 already does the multiply (`p -= 2;` on `int *` → 2*2 = 4)
— this batch just made TASM accept the emitted asm.

## `char c = a[K];` — skip widening peephole

Fixture `567` (`char a[3] = {'x', 'y', 'z'}; char c = a[1];`) —
`emit_array_index_to_ax` for a char global array loads `mov al,
byte ptr [_a+K]` and then sign-extends with `cbw`. When the
destination is itself a char slot (byte store truncates back),
the `cbw` is purely wasted — BCC skips it.

`emit_assign_local`'s stack branch now special-cases this
shape: char-local target + char-array constant-index source on
a global. It emits `mov al, byte ptr DGROUP:_a+K; mov byte ptr
[bp-N], al` — 6 bytes — without the cbw. Other code paths
through `emit_expr_to_ax` still widen because their consumers
(arithmetic, ax-passing) need a full int.

## `if (g & K)` bit test

Fixture `569` (`int g; if (g & 1) ...`) — BCC uses the `test`
instruction to set ZF directly from a masked memory read,
avoiding the load-into-register-then-`and`-then-test path.
`emit_cond_test` now special-cases `BinOp::BitAnd` with an int
global LHS and a constant RHS: emit `test word ptr DGROUP:_g,
K` (`F7 06 lo hi imm_lo imm_hi`, 6 bytes), then the standard
`jne/je` pair. A new tasm IR variant `TestGroupSymImm16`
encodes it.

## `a += b;` between two int globals

Fixture `571` (`int a; int b; a += b;`) — `emit_compound_
assign` now handles the int-global + int-global case with `mov
ax, [_b]; add word ptr [_a], ax`. The store-back uses the
generic Grp1 r/m16, r16 form (`01 06 lo hi` for ADD; `29 06 lo
hi` for SUB). Two new tasm IR variants `AddGroupSymReg16` /
`SubGroupSymReg16` encode these.

### Char-local array layout (resolved in batch 86)

Probed `char a[3]; char c; c = a[1];` and discovered BCC's
local frame allocator pads char arrays to even byte boundaries,
leaving the byte after the array as padding. Probe was replaced
with the int-array variant (fixture 570) and the underlying
padding rule was reverse-engineered when fixture 577 surfaced
the same issue — see `char s[3]; char *p; ...` below.

## Free passes (no code changes needed)

Three more probes hit existing paths byte-exactly:

- `572` — `if (a || b)` between two int globals: the bare-ident
  short-circuit lowering already routed through `emit_cond_
  branch` and the established or-skeleton.
- `573` — `sizeof(int *)` returns 2: the parse-time
  `parse_type_name` already handles the `int *` declarator and
  `Type::Pointer(_).size_bytes()` is 2.
- `574` — `continue` inside a `while`: the planner's
  `continue_target_slot = check_slot` mapping for while was
  already correct (continue → top of test). Distinct from the
  for-loop case fixed in fixture 558.

## `char s[3]; char *p; p = s; p++; return *p;`

Fixture `577` (char pointer increment over a local char array)
forced the char-local-array layout question that was deferred
after fixture 570. Diff showed `83 ec 04` (our `sub sp, 4`) vs
BCC's `83 ec 06`: BCC rounds each array slot up to an even
number of bytes regardless of element alignment, so `char s[3]`
takes a 4-byte slot with `s[0]..s[2]` at `bp-4..bp-2` and `bp-1`
left as padding. Fixed in `crates/bcc/src/codegen/locals.rs`
by bumping `slot_size` by 1 when the type is `Array { .. }` and
`slot_size % 2 == 1`. Retroactively resolves the deferred char-
local-array layout note.

## Free passes (batch 86)

Two more probes hit existing paths byte-exactly:

- `575` — `int g = 42; int x = g;` local init from a global: the
  initializer path already routes through `emit_assign` and the
  global-load codegen.
- `576` — `r = (a == b);` comparison-as-value: `emit_eq` already
  materializes the boolean into a register and the store path
  was unchanged.

## Free passes (batch 87)

Three more probes hit existing paths byte-exactly with no code
changes:

- `578` — `if (a <= b) return 1; return 0;` (int signed less-
  than-or-equal in if-cond): `emit_cond_branch` already lowers
  `<=` to `cmp; jg <else>` with the correct signed predicate.
- `579` — `return a >= b;` (int signed greater-than-or-equal as
  return value): `emit_ge` materializes the boolean into AX and
  the return path was unchanged.
- `580` — `int f(int a, int b) { return a + b; }` (two-int-arg
  callee + call from `main`): the cdecl call/return path
  already pushes args right-to-left and the two-arg parameter
  frame layout came out byte-exact (we'd previously tested 1-
  and 3-arg variants but not 2).

## Free passes (batch 88)

Three more probes hit existing paths byte-exactly with no code
changes:

- `581` — `if (a && b)` (bare-ident logical-and between two int
  globals): the and-skeleton (`emit_cond_branch` + cascading
  branch on zero) handles this just like the bare-ident or-form
  fixed in fixture 572.
- `582` — `g--;` (int global postdec used as statement): the
  postdec lowering already maps to `dec word ptr [_g]` for the
  statement context.
- `583` — `if (!(a < b))` (logical-not applied to a relational
  expression): `emit_cond_branch` already inverts the
  predicate, so `!(a < b)` lowers to `cmp; jl <then>` (the not-
  taken edge falls through to the else).

## `int x /= K;` / `int x %= K;` on register-resident locals

Fixtures `584` (`x /= 2`) and `585` (`x %= 5`) — `emit_compound_
assign_reg` previously panicked for `Div`/`Mod`. The pattern
BCC uses for an int-register local x in SI is:

```text
  mov bx, K       ; divisor (DX is clobbered by `cwd`)
  mov ax, si      ; dividend
  cwd             ; sign-extend AX into DX:AX
  idiv bx         ; AX=quotient, DX=remainder
  mov si, ax      ; for /= ; or  mov si, dx  for %=
```

The new arm in `emit_compound_assign_reg` materializes the RHS
into BX (constant or register-source), runs the `mov ax/cwd/idiv`
prefix, and stores AX (for `/=`) or DX (for `%=`) back to the
target register. A new tasm IR variant `IdivReg16` encodes `idiv
<reg>` as `F7 (mod=11 /7 r/m=reg)`; previously only the bp-
relative form was supported.

## Free passes (batch 89)

- `586` — `char a; char b; a=1; b=2; return a + b;` (char + char
  return): the char-add lowering through AL/AH widening already
  handled this; both chars promote to int per C90, the sum lands
  in AX, and `ret` returns it.

## `f(a[K])` — direct `push word ptr [bp-N]` arg

Fixture `589` (`int f(int x); int a[3]; f(a[1]);`) — the int-
arg push path was emitting `mov ax, word ptr [bp-N]; push ax`
(4 bytes) while BCC emits `push word ptr [bp-N]` (3 bytes)
directly for memory-operand args. Added `try_direct_arg_push`
to `emit_call`: when the arg is a const-index array element on
a stack-resident int/ptr array, skip the AX round-trip and emit
the `push m16` form. The broader cases (bare ident local, bare
ident global) also use this shape in BCC but aren't currently
exercised by any fixture; the peephole was kept narrow to avoid
churning unrelated callers.

## Free passes (batch 90)

Two more probes hit existing paths byte-exactly:

- `587` — descending `for (i = 10; i > 0; i--)`: the for-loop
  planner already handles the postdec step and `i > 0` test
  shape.
- `588` — `int a; int b; ... return a > b ? a : b;` (ternary
  over int globals): `emit_ternary` materializes both branches'
  values into AX correctly.

## `int *p; p[K] = v;` — register-pointer indexed write

Fixture `590` (`int g; int *p; p = &g; p[0] = 42;`) — the
pointer-subscript-write path in `emit_array_assign` previously
only handled long pointees, falling through to the generic
"array should be stack-resident" panic for int pointers.
Extended the path: when the pointee fits in a word, emit `mov
<width> ptr [<reg>(+<off>)], <value>` directly. For `byte_off
==0` the address is `[<reg>]`; otherwise `[<reg>+<off>]`. The
non-constant RHS case still panics with an explicit "no fixture"
marker.

## `if (f())` — call as boolean condition

Fixture `591` — `emit_zero_test` previously only handled `Ident`
and `AssignExpr`. Added a `Call` arm that lowers to `call near
ptr _f; or ax, ax`, matching BCC's pattern (the call leaves the
return value in AX and `or` sets ZF for the conditional branch).

## `*K` peephole — `shl ax, 1` for power-of-2 K

Fixture `592` (`int f(int x) { return x * 2; } int main(void) {
return f(g(3)); }`) — `emit_op_with_source` for `BinOp::Mul`
previously panicked for any immediate. BCC's pattern for `* K`
with K a small power of two is to unroll into `shl ax, 1`
repeated (no `imul` involved). Added that peephole; non-power-
of-2 immediates still panic with an explicit "no fixture"
marker (BCC's shape in that case is `mov dx, K; imul dx`).

## `n + sum(n - 1)` — RHS-call evaluation order

Fixture `593` (simple recursion `int sum(int n) { if (n <= 0)
return 0; return n + sum(n - 1); }`) — `emit_expr_to_ax` for
binary ops previously always evaluated LHS into AX first. BCC's
pattern when the RHS is a call is right-first:

```text
  mov ax, si      ; (compute call arg from LHS-shared reg)
  dec ax
  push ax
  call near ptr _sum
  pop cx
  push ax         ; save call result
  mov ax, si      ; reload LHS
  pop dx          ; restore saved result
  add ax, dx
```

A call clobbers AX, so evaluating it first and saving the
result before re-loading LHS avoids the extra `push/pop` of an
already-in-AX value. Added the RHS-call branch to the BinOp arm
of `emit_expr_to_ax`: when `right.kind` is `Call`, emit RHS to
AX, push, emit LHS to AX, pop into DX, then apply the op with
DX as the source operand.

## `*p = v` — non-constant RHS for int/char pointees

Fixture `595` (`int x; int *p; p = &x; *p = *p + 1;`) — the
`*p = v` path on a register-resident pointer previously
required a constant RHS. Extended the path: when the RHS isn't
const-foldable, `emit_expr_to_ax(value)` materializes the
value in AX/AL, then `mov <width> ptr [<reg>], ax/al` stores
it. A new tasm IR variant `MovSiPtrReg16` encodes `mov word
ptr [si], <reg16>` as `89 (mod=00 reg=<src> r/m=100)`; only
the immediate form was previously supported.

## Free passes (batch 92)

- `594` — `int x; int y; x = 16; y = 2; return x >> y;`
  (signed `int >> int` with a non-constant shift count): the
  existing shift-by-CL lowering (`mov cl, byte ptr [y]; sar ax,
  cl`) already byte-matches BCC.

## Free passes (batch 93)

Three more probes hit existing paths byte-exactly with no code
changes:

- `596` — `int *p; p = &g; return p[0];` (int-pointer subscript
  read, K=0): the deref-through-register read path already
  emits `mov ax, word ptr [si]`, identical to `*p` since K=0.
- `597` — `int f(int *p) { return *p; } int main(void) { int x;
  x = 7; return f(&x); }` (passing `&local` as an int-pointer
  arg): `&x` forces `x` to a stack slot, `lea ax, word ptr [bp-
  N]` materializes the address, and the existing call path
  pushes it.
- `598` — `int main(void) { int x; x = 5; return x * x; }`
  (square of a local): the `imul <src>` path with a non-
  immediate source already handles this (both operands resolve
  to the same register-resident local — `mov ax, si; imul si`).

## `while (1)` — frame-less infinite loop

Fixture `599` (`while (1) { if (g >= 3) break; g++; }`) — when
the while condition is a const non-zero, BCC elides both the
trampoline jump and the check label, leaving just `body_label /
body / jmp body_label`. Added a constant-cond branch at the
top of `emit_while`: when `try_const_eval(cond)` is `Some(v)`
with `v != 0`, emit the body with `continue_target = body_slot`
and a trailing `jmp body_slot`. The break-target label is still
gated on `body_has_break`.

## `<stack-local> = &<global>;` — direct-store peephole

Fixture `601` (`int *p; int *q; p = &g; q = &g;`) — BCC stores
the symbol's offset directly into the stack slot with `C7 46 dd
lo hi` + FIXUPP, saving the AX round-trip used for runtime
addresses. Added the peephole in `emit_assign_local`: when the
RHS is `AddressOf(global_sym)` and the destination is a non-
char stack slot, emit `mov word ptr [bp+off], offset DGROUP:_sym`.
A new tasm IR variant `MovBpRelOffsetGroupSym` encodes that
shape (sibling of `MovGroupSymOffsetGroupSym` used by global-
to-global address writes).

## Free passes (batch 94)

- `600` — `int a, b, c; a = 1; b = 2; c = 3; return a + b + c;`
  (multi-decl int locals on one line): the parser already
  handles comma-separated declarators in a single decl, and the
  locals planner allocates each in declaration order.

## Free passes (batch 95)

Three more probes hit existing paths byte-exactly with no code
changes:

- `602` — `return (a + b) * 2;` (parenthesized sum then `* 2`):
  the runtime sum lands in AX via `add ax, <src>` and the new
  `* 2` peephole from batch 91 turns the constant multiply
  into `shl ax, 1`.
- `603` — `int a; a = 5; ++a; ++a; return a;` (sequential
  preincs on the same local): each `++a;` lowers to a register
  `inc` independently.
- `604` — `char c; int n; c = 5; n = c; return n;` (int =
  char widening through assignment): `emit_assign_local`
  already loads the char with sign extension via `cbw` and
  stores the widened word.

## Free passes (batch 96)

Three more probes hit existing paths byte-exactly with no code
changes:

- `605` — `int x; int y; x = 12; y = 10; return x | y;` (int
  OR between two locals): the bitwise-op path already emits
  `mov ax, <left>; or ax, <right>` for int operands.
- `606` — `void f(void) { return; } int main(void) { f(); return
  0; }` (void function with bare return): the void-return path
  already drops the value-load and just emits the exit jump.
- `607` — `int f(char c) { return c + 1; } int main(void) {
  return f(5); }` (int return from `char + 1` arithmetic): the
  char-param load through DL/CBW widens to AX, then `inc ax`
  computes the int return value.

### Deferred from batch 96

Probed `char f(int x) { return x + 1; }` (a char-returning
function whose body computes `x + 1` from an int param). BCC
truncates the int param at the load — `mov al, byte ptr [bp+4];
inc al` — instead of `mov ax, [bp+4]; inc ax`. Both produce
the same low byte, but BCC's shape is 1 byte longer (`inc al`
is 2 bytes vs `inc ax`'s 1) and matches the function's char
return type. Implementing this would require routing char-
returning function bodies through AL where the source is a
narrow expression. Probe replaced with the `int f(char c)`
direction (mirror image) — that one works through existing
char-param widening.

## `c & K` — `and ax, imm16` accumulator form

Fixture `609` (`char c; c = 15; return c & 4;`) — after `mov
al, byte ptr [bp-1]; cbw`, BCC emits `25 04 00` (`and ax,
imm16`, the AX-specific accumulator form). Our tasm parser
previously accepted only `and ax, <symbol-or-mem>` forms. Added
the `AndAxImm16` IR variant with encoding `25 lo hi` plus a
parser entry that fires when LHS is AX and RHS is a 16-bit
immediate. 3 bytes vs the 4-byte generic `81 E0 lo hi`.

## Free passes (batch 97)

- `608` — `for (i = 0; i <= 5; i++) sum = sum + i;` (`<=` in
  for-test): the for-loop check lowers `<=` to `cmp; jg
  <break>` correctly.
- `610` — `char c; char *p; p = &c; return *p;` (char pointer
  to a stack char-local): `&c` forces `c` to a stack slot,
  `lea ax, [bp-1]` materializes the address, and `mov al,
  byte ptr [si]` reads through the pointer.

## `x | K` / `x ^ K` — `or/xor ax, imm16` accumulator forms

Fixtures `611` (`return x | 8;`) and `612` (`return x ^ 3;`) —
mirrors of the batch-97 `and ax, imm16` fix. BCC uses the AX-
specific 3-byte accumulator forms: `0D lo hi` for OR and `35
lo hi` for XOR. Added `OrAxImm16` and `XorAxImm16` IR variants
with their parser entries.

## `x % K` / `x / K` — materialize divisor in BX

Fixture `613` (`return x % 7;`) — the `idiv` instruction has
no immediate form. BCC materializes the divisor in BX with
`mov bx, K`, then `cwd; idiv bx`. `emit_op_with_source` for
`Mod`/`Div` previously panicked on immediate sources. Added
the imm path: emit `mov bx, K; cwd; idiv bx`, then for `Mod`
append `mov ax, dx` (remainder). Symmetric with the compound
`/= K` path landed in fixture 584.

## `x * K` — non-power-of-2 path

Fixture `615` (`return x * 3;`) — the batch-91 `* K` peephole
only covered powers of two (`shl ax, 1` unrolling). For other
constants BCC materializes K in DX and uses single-operand
`imul dx`. Added the non-power-of-2 arm: `mov dx, K; imul dx`.

## Char-ident RHS — same RHS-first shape as `Call`

Fixture `616` (`int f(int a, char b) { return a + b; }`) —
loading a char clobbers AX through the `mov al, byte ptr ...;
cbw` widen, so BCC evaluates the char RHS first, pushes the
widened result, then loads the int LHS, pops into DX, and
applies the op. Previously our `emit_binary_right` had a
char-on-right pattern that produced a functionally equivalent
result through `push ax / mov al,...; cbw / mov dx, ax / pop
ax`, which is 2 bytes longer because of the extra `mov dx,
ax`. Extended the batch-92 RHS-clobbers-AX check (originally
just `Call`) to also fire on a char-typed `Ident` RHS, routing
through the cleaner `evaluate RHS / push / evaluate LHS / pop
dx / op` shape.

## Free passes (batch 99)

- `614` — `return x / 7;` (int divide by const): the batch-98
  `Div` immediate path already covers this — `mov bx, K; cwd;
  idiv bx` with no `mov ax, dx` follow-up (quotient is already
  in AX).

## `while (x--)` — postdec as boolean condition

Fixture `619` — `emit_zero_test` previously handled `Ident`,
`AssignExpr`, and `Call`. Added a `Post`-update arm that
materializes the value-then-side-effect sequence via
`emit_expr_to_ax` and follows with `or ax, ax`. BCC's shape
for `x--` in a boolean context (with `x` in SI) is `mov ax,
si; dec si; or ax, ax` — exactly what the existing postdec
lowering produces when its result is used.

## Free passes (batch 100)

- `617` — `int x; x = 0; if (!x) return 1; return 0;` (`!x`
  on an int local in if-cond): `emit_cond_branch` already
  inverts the test through the standard `or ax, ax; je
  <then>` shape.
- `618` — `int x; int r; x = 0; r = !x; return r;` (`!x` as
  a value): `emit_logical_not` materializes `1` or `0` into
  AX based on the operand's zero-test.

## Chained `&&` / `||` — non-final operand short-circuit

Fixtures `620` (`if (a && b && c)`) and `621` (`if (a || b ||
c)`) — `emit_cond_branch` previously panicked with "nested
`&&`/`||` operators not yet supported". The recursive `&&`
case already inherited `(true_slot, false_slot)` for the right
operand, so chained `&&` was already correct once the assert
was lifted. The Or case was asymmetric: it passed `(None,
false_slot)` to the right, expecting the caller to emit the
true label immediately after — that's the "right falls through
on true" optimization for flat `a || b`. For chained
`(a || b) || c` (left-associative), the inner Or's right `b`
isn't the final operand: between b's evaluation and the true
label the outer Or emits `c`'s test, so b's "fall through on
true" lands in the middle of c's test. Fixed by distinguishing
final vs non-final Or via the outer `false_slot`: when
`false_slot.is_some()` we're at the top of an if-cond chain
(right can fall through); when `false_slot.is_none()` we're
inside another Or's LHS (right must jump on true to the
inherited `true_slot`).

## Free passes (batch 101)

- `622` — `char c; c = 1; c |= 32; return c;` (char compound
  OR with constant): the existing char-register compound-
  bitwise path (`or <reg8>, K`) already handled this — sibling
  of fixture 556's `c &= 31`.

### Deferred from batch 101

Probed `int main(void) { int a[3] = {10, 20, 30}; return
a[1]; }` (int local array with initializer list). BCC stores
the initializer values in a `_DATA`-segment `d@w` block and
copies them into the stack frame at function entry via
`N_SCOPY@` (the same helper used for struct copies > 4 bytes).
Our codegen panics with "non-constant init for non-int-like
type Array { elem: Int, len: 3 }". Implementing this would
need the init-data emitter plus the prologue-time
`push ss; lea ax, [bp-N]; push ax; push ds; mov ax, offset
d@w; push ax; mov cx, <size>; call N_SCOPY@` shape — sizable.
Probe replaced with the char-compound-OR variant.

## `c -= K` — BCC normalizes as `add al, -K`

Fixture `623` (`char c; c -= 3;`) — BCC canonicalizes char
compound subtract as `add al, -K` (encoded `04 FD` for `c -=
3`) rather than `sub al, K` (`2C 03`). Both are 2 bytes and
produce the same result modulo 256, but BCC picks the `add`
form consistently. Updated the char compound `+=`/`-=` arm in
`emit_compound_assign_reg`: for `Sub` with K != 1, emit
`add al, -K` (negation taken as i8). Char compound `+=` keeps
emitting `add al, K`.

## `add ax, word ptr [di]` — second-pointer dereference

Fixture `625` (`int *p; int *q; ... return *p + *q;`) — BCC
enregisters the two pointer locals into SI and DI; the
sum lowers to `mov ax, [si]; add ax, [di]`. Our tasm
previously only had `AddAxFromSiPtr` (`03 04`). Added the DI
companion `AddAxFromDiPtr` (`03 05`, ModR/M 05 = mod=00
reg=AX r/m=101 ([DI])) and its parser entry.

## Free passes (batch 102)

- `624` — `char c; c ^= 32;` (char compound XOR with const):
  the bitwise-compound path already emits `xor <reg8>, K`
  (sibling of fixture 556's `c &= 31` and 622's `c |= 32`).

## `x >> K` / `x << K` — unroll for K ≤ 3

Fixture `627` (`return x >> 3;`) — BCC unrolls expression-
context shifts by 1, 2, or 3 into `<sar|shr|shl> ax, 1`
repeated, even when K=3 (where the unrolled 6 bytes is longer
than the CL form's 4 bytes). For K ≥ 4 BCC switches to `mov
cl, K; <op> ax, cl`. Probed K = 1, 2, 3, 4, 5, 8 to pin the
threshold. Updated the `Shl`/`Shr` arm of `emit_op_with_source`:
when the source is an immediate K in 1..=3, emit K copies of
`<op> ax, 1`; otherwise fall back to the existing CL form.
(Note: compound shifts like `x <<= 4` keep using `mov cl, K;
shl reg, cl` per fixture 537 — this asymmetry between
expression and compound context is BCC's, not ours.)

## `*p = x` — register-source direct store

Fixture `628` (`int f(int x, int *p) { *p = x; return x; }`)
— with `p` enregistered in DI and `x` in SI, BCC stores SI
directly to `[di]` via `89 35` (`mov [di], si`) — skipping the
AX round-trip our codegen had used after batch 92. Added a
peephole in the `*p = v` register-pointer path: when the RHS
is a non-char ident on a register-resident local, emit `mov
<width> ptr [<addr_reg>], <src_reg>` directly. Also added the
`MovDiPtrReg16` tasm IR variant (`89 mod=00 reg=<src> r/m=101
([DI])`) to encode it; only the SI form existed previously.

## Free passes (batch 103)

- `626` — `return x << 4;` (int shift-left by 4): falls into
  the CL form (since K=4 ≥ 4 per the new unroll cutoff above)
  — `mov cl, 4; shl ax, cl`.

## `x - K` — BCC normalizes as `add ax, -K`

Fixture `630` (`int x; x = 10; return x - 5;`) — BCC
canonicalizes int subtract-const as the AX-accumulator
`add ax, imm16` form (`05 FB FF` for `x - 5`) rather than
the `sub ax, imm8sx` form (`83 E8 05`). Both are 3 bytes
when K fits in i8, but `add ax, -K` keeps using 3 bytes for
the full 16-bit range while `sub ax, K` would grow to 4
bytes via `81 E8 lo hi` once K exceeds 127. Mirror of the
batch-102 `c -= K` → `add al, -K` fix. Updated the `Sub` arm
of `emit_op_with_source` to emit the negated-add form for
immediate sources.

## Free passes (batch 104)

- `629` — `int x; x = 13; return x & 7;` (int AND with const
  small enough to fit imm16): the `AndAxImm16` form added in
  batch 97 already handles this (`25 07 00`).
- `631` — `int a; int b; ... return (a + b) / 2;` (sum then
  divide-by-const): the runtime add lands in AX; the const
  divide goes through the batch-98 `mov bx, 2; cwd; idiv bx`
  path. (Note: BCC does NOT use a `sar` peephole for divide
  by power of 2 here — same shape as `/ 7`.)

## `int x = -K;` — mask negative initializer to width

Fixture `632` (`int x = -5; return x;`) — `try_const_eval`
returns a u32 (`-5` becomes 0xFFFFFFFB = 4294967291 decimal),
which leaked through the stack-init `mov word ptr [bp-N],
{v}` write and produced an out-of-range imm. Fixed by masking
`v & 0xFFFF` (int) or `v & 0xFF` (char) at the stack-init
emit site. The global-init path already did this; the local-
init path didn't.

## `c *= K` (power-of-2 K) — round-trip + `shl al, 1` unroll

Fixture `633` (`char c; c = 3; c *= 4;`) — char compound
multiply previously hit the "char compound on byte target not
yet supported" assert. BCC's pattern for K a small power of
two is round-trip through AL with unrolled `shl al, 1`: `mov
al, <reg>; shl al, 1; shl al, 1; mov <reg>, al`. Added that
arm to `emit_compound_assign_reg` next to the char-shift
sibling. Non-power-of-2 K still panics (BCC would presumably
use `mov dl, K; imul dl` — no fixture yet).

## Free passes (batch 105)

- `634` — `for (i = 1; i <= 10; i++) { if (i > 5) break; sum +=
  i; }` (for + nested-if + break + compound-add): the existing
  for-loop body emission already routes `break` from inside a
  nested if to the loop's break_target_slot, and the compound
  `+=` path emits `add <reg>, <op>` in place.

## `while (*p)` — deref through reg-pointer as boolean

Fixture `636` (`char *p; while (*p) { n++; p++; }`) —
`emit_zero_test` panicked because the condition is `Deref(Ident
p)`, not bare `Ident`. BCC's pattern with `p` enregistered in SI
is `cmp byte ptr [si], 0; jne ...` directly (no AX round-trip).
Added a `Deref(Ident reg-pointer)` arm to `emit_zero_test` that
emits `cmp <width> ptr [<reg>], 0` with the width from the
pointee. New tasm IR variant `CmpByteSiPtrImm8` encodes the
byte form (`80 3C ii`).

## Free passes (batch 106)

- `635` — `char c = -1; return c;` (char neg-literal init):
  the batch-105 char-init mask (`v & 0xFF`) handles the
  negative value cleanly — `mov byte ptr [bp-1], 255`.
- `637` — `int x; int y; x = 5; y = x * 3; return y;` (int
  mul-const stored to local): the batch-99 `mov dx, 3; imul
  dx` path routes through AX, then `mov word ptr [bp-N], ax`
  stores the result.

## `c /= K` / `c %= K` — char compound divide/modulo

Fixture `640` (`char c; c = 12; c /= 4;`) — two related
changes:

1. **Codegen**: added the char compound div/mod arm in
   `emit_compound_assign_reg`. Pattern (BCC):
   `mov al, <reg>; cbw; mov bx, K; cwd; idiv bx; mov <reg>,
   <al|dl>`. The cbw widens char to AX, idiv produces quotient
   in AX and remainder in DX, then the low byte of the chosen
   result stores back. Shift-unroll wouldn't match signed
   semantics (rounding differs for negative values).
2. **Allocator**: BCC drops DL from the char pool when the
   function body contains any signed div/mod, because the
   `cwd` preceding `idiv` clobbers DX. Probed by comparing
   our output to BCC's — our planner had c in DL, BCC had it
   in CL. Added `body_has_div_or_mod` walk and a new
   `CHAR_POOL_DIV = [CL, BL]` variant that's selected when
   the body has division.

## Free passes (batch 107)

- `638` — `int x; x = 5; if (x != 0) return 1; return 0;`
  (int `!= 0` in if-cond): `emit_cond_branch` already emits
  `cmp ax, 0; jne ...` for the comparison-with-zero pattern.
- `639` — `int a; int b; ... if (a != b) return 1;` (int !=
  int): the standard cmp-as-branch path lowers `!=` to `cmp;
  jne` over the operand pair.

## `char f(char c) { return c; }` — no widen on char return

Fixture `643` — char-returning function with a char-typed
return value. Our `emit_return_value_load` had a special
arm for unsigned char globals (no widen) but otherwise fell
through to `emit_expr_to_ax`, which widens char idents to AX
via `cbw`. BCC's ABI for char-returning functions is "AL
holds the value, AH is garbage" — the caller widens after
the call if it needs an int. Added a signed-char-return arm
that loads the char directly into AL (without cbw): `mov al,
byte ptr [bp+N]` for stack chars or `mov al, <reg8>` for
register chars. This addresses the deferred batch-96 item
about char-returning function bodies through AL.

## Free passes (batch 108)

- `641` — `do { x++; } while (x != 5);` (do-while with `!=`
  test): the do-while lowering and `!=` branch combine cleanly.
- `642` — `char c; c = 16; c >>= 2;` (char compound right
  shift, K=2): the existing char compound shift path unrolls
  `sar al, 1` (signed) twice — sibling of fixture 535's
  `c <<= 2`.

## Nested BinOp as RHS — extend RHS-clobbers-AX path

Fixture `645` (`return x + y * 2;`) — when the right operand
of a binary op is itself a non-constant BinOp (e.g., `y * 2`),
its evaluation lands the result in AX. Previously
`resolve_operand_source` panicked on a BinOp RHS. BCC's
pattern matches the call-RHS path from batch 92:

```text
  mov ax, [bp-4]    ; y
  shl ax, 1         ; y * 2
  push ax           ; save RHS
  mov ax, [bp-2]    ; x
  pop dx            ; recover RHS
  add ax, dx        ; x + (y*2)
```

Extended the `rhs_clobbers_ax` check in `emit_expr_to_ax`'s
BinOp arm to also fire when `right.kind` is a non-constant
BinOp. That routes through the existing RHS-first / push /
LHS / pop dx / op-with-DX sequence.

## Free passes (batch 109)

- `644` — `int x; x = 5; x += x;` (self-compound-add): the
  compound-add path emits `add <reg>, <reg>` cleanly even
  when source and destination are the same.
- `646` — `if (x == 5 || x == 10)` (logical OR with two `==`
  cmps): the cmp-as-branch path lowers each `==` to `cmp; je`
  and the OR-skeleton wires them together.

## `cmp <reg16>, word ptr [bp+N]` — generic register-vs-stack

Fixture `648` (`for (i = 0; i < n; i++)` with i in SI and n at
`[bp-2]`) — tasm previously only had `CmpAxBpRel` and
`CmpDxBpRel`. Added the generic `CmpReg16BpRel` IR variant
(`3B (mod=01 reg=<r> r/m=110) dd`), which handles SI/DI/BX/CX.
AX and DX keep their dedicated variants since the long-compare
scaffolding references them by name.

## `<stack-int> = <reg-int>++` — direct-store postinc

Fixture `649` (`r = x++` with x in SI and r at `[bp-2]`) — BCC
stores SI directly to the stack slot, then applies the
inc/dec: `mov word ptr [bp-2], si; inc si` (6 bytes vs our 7
through AX). The generic `emit_update_to_ax` had a byte/word
register confusion (`mov ax, dl` is invalid x86) and emitted
the side effect before the store. Two fixes:

1. `emit_update_to_ax` now handles byte registers via `mov
   al, <reg8>; cbw` instead of the bogus word mov.
2. Added an Assign-statement peephole in `emit_assign_local`:
   when the RHS is `Update { Post, target: reg-int-local }`
   and the destination is a non-char stack slot, store the
   pre-update register directly and then inc/dec it.

(Note: the matching char variant — `r = c++` with c in DL —
is still 2 bytes off because the store happens after the
inc instead of before. Deferred until a fixture forces a
deeper restructure that defers the side effect to after the
store.)

## Free passes (batch 110)

- `647` — `return a * b + c;` (three-way arith, mul then add):
  combines the batch-99 `imul <src>` path with the batch-109
  RHS-clobbers-AX swap.

## `x *= y` — `imul <mem>` directly for memory-source RHS

Fixture `651` (`int x; int y; x *= y;` with x in SI and y at
`[bp-2]`) — BCC uses `imul word ptr [bp-2]` directly rather
than materializing the operand in DX first. Our existing
compound-mul path always did the DX round-trip (`mov dx, src;
imul dx`), which costs 2 extra bytes for a memory source.
Updated the `BinOp::Mul` arm of `emit_compound_assign_reg`:
when the resolved source is `Local`/`Global`/`GlobalOffset`,
emit `imul <mem>` directly; constants and registers still
use the DX path.

## Free passes (batch 111)

- `650` — `int x; int y; x = 5; y = -x; return y;` (neg of
  var stored to another local): `emit_unary_neg` materializes
  the negation in AX and the assign-local path stores it.
- `652` — `if (a + b > 10)` (if with arith compare): the
  comparison's left operand is a non-constant BinOp; the
  comparison path materializes both operands and emits the
  standard `cmp; jle <skip>` form.

## `x /= y` / `x %= y` — `idiv <mem>` directly

Fixtures `653` (`x /= y`) and `654` (`x %= y`) — mirror the
batch-111 `imul <mem>` fix for division. BCC's compound divide
on a register local with a memory-resident RHS emits `idiv
word ptr [bp-N]` directly rather than materializing in BX
first. Updated the `BinOp::Div | BinOp::Mod` arm of
`emit_compound_assign_reg`: when the resolved source is
`Local`/`Global`/`GlobalOffset`, emit `idiv <mem>` directly;
constants and registers still use the BX path.

## `and si, word ptr [bp+N]` — generic AND reg-vs-stack

Fixture `655` (`x &= y` with x in SI, y at `[bp-2]`) — tasm
had `AndAxBpRel` and `AndDxBpRel` but no SI/etc. variant.
Added the generic `AndReg16BpRel` IR variant (`23 (mod=01
reg=<r> r/m=110) dd`) — sibling of the batch-110
`CmpReg16BpRel`. AX keeps its dedicated variant.

## `or` / `xor` reg-vs-stack and non-constant compound shift

Fixtures `656` (`x |= y`), `657` (`x ^= y`), `658` (`x <<=
y`) — three sibling fixes:

- Added `OrReg16BpRel` (`0B mod=01 reg=<r> r/m=110 dd`) and
  `XorReg16BpRel` (`33 ...`) tasm IR variants, mirrors of
  `AndReg16BpRel` from batch 112.
- Extended the `Shl`/`Shr` arm of `emit_compound_assign_reg`
  to accept a non-constant RHS: load the low byte of the RHS
  into CL with `mov cl, byte ptr <src>`, then shift the
  register. BCC pattern: `mov cl, byte ptr [bp-2]; shl si,
  cl`.

## `add` / `sub` reg-vs-stack for compound `+=` / `-=`

Fixtures `659` (`x >>= y`, free pass via the batch-113
non-constant shift extension), `660` (`x -= y` with x in SI,
y at `[bp-2]`), and `661` (`x += y`) — completed the
arithmetic siblings of the batch-112/113 bitwise BpRel set.

- Added `AddReg16BpRel` (`03 mod=01 reg=<r> r/m=110 dd`) and
  `SubReg16BpRel` (`2B ...`) tasm IR variants. Sibling of
  `AndReg16BpRel`/`OrReg16BpRel`/`XorReg16BpRel`. AX keeps
  its accumulator-form `AddAxBpRel`/`SubAxBpRel` variants.
- Parser entries gated on `!matches!(reg, Reg16::Ax)` so the
  AX accumulator paths still take precedence (AX uses the
  shorter `03 46 dd`-equivalent? no: AX has its own dedicated
  variant, the gate is for routing only).
- No codegen change was needed — the existing
  `emit_compound_assign_reg` `BinOp::Add`/`Sub` arm already
  emits `add <reg>, word ptr [bp+N]` / `sub <reg>, word ptr
  [bp+N]` as text; only the parser+encoder needed to
  recognize the non-AX form.

## Global `++` in condition, char global postinc/preinc edge cases

Fixtures `971` (`int g; if (g++) return 7;` — int global
postinc as boolean condition), `972` (`char g; return g++;`
— char global postinc in return), `973` (`char g; return
++g + 1;` — char global preinc as arithmetic operand).

All three already work end-to-end via the batch 215–217
infrastructure plus the existing zero-test paths:

- 971: `if (g++)` exercises the `emit_zero_test` Update-Post
  arm (fixture 619) — the post-update value is loaded into
  AX, the side effect mutates `g` in memory, and `or ax,
  ax` sets ZF on the *pre*-update value. Combined with the
  global-aware `emit_update_to_ax` fast-path (batch 215),
  this works for global Update targets the same way it
  already did for local ones.
- 972/973: char globals in return / arithmetic context.
  The `emit_update_to_ax` Post/Pre arms emit `mov al, mem;
  inc al; mov mem, al; cbw` (Pre) or `mov al, mem; inc
  mem; cbw` (Post), and the return / `+ 1` consumer feeds
  off AX. No deferred-side-effect peephole needed since
  there's no intermediate store.

**Recorded finding (deferred):** Probed `dbl(g++)` —
`int dbl(int x) { return x + x; } ... return dbl(g++);` —
expected bytes match for the codegen but the **public
symbol list ordering** differs. BCC emits `_dbl, _main, _g`
(functions in *source order*, globals last) while we emit
`_main, _dbl, _g` (functions in reverse-source / LIFO
order). Existing fixture 138 (`int f(...) {...} int
main(void) { f(1, 2, 3); }`) shows BCC emits `_main, _f`
— the reverse-source order matches our current behavior.
The two orderings contradict, so there's a BCC heuristic
we haven't reconstructed yet. Replaced the `dbl(g++)`
probe with the `if (g++)` boolean form which doesn't
trigger the function-public-order codepath. To
investigate: try multiple call-site shapes (called-by-
main vs not, with-globals vs not, multiple callees) and
look for the partition that selects source-order vs
reverse-source.

## Global `++`/`--` in return and arithmetic

Fixtures `968` (`return g++;` — int global postinc in return),
`969` (`return ++g;` — int global preinc in return), `970`
(`return g++ + 1;` — int global postinc as an arithmetic
operand).

All three work end-to-end after batches 215/216. In return
position there's no follow-on `mov [bp-2], ax` store to
defer past, so the generic `emit_update_to_ax` shape (load
+ inc together for post; inc + load for pre) lands in AX
and the return path consumes it directly. No deferred-side-
effect peephole is needed because the function-exit jump
follows immediately.

For 970, BCC emits `mov ax, g; inc word ptr g; add ax, 1`
— the same load+inc pair from `emit_update_to_ax`, with
the binary `+ 1` becoming the standard `add ax, K` step.
The captured pre-update value flows into the arithmetic
unchanged. Byte-for-byte match.

Conclusion: the deferred-side-effect peephole from 963 /
966 is specific to the `<stack-local> = <global>++/--`
shape — when the use is a return, an arithmetic operand,
or a function call, the side effect naturally happens
before the value flows further, so the generic load+mutate
ordering matches BCC.

## Global `--`/`++` in expression — postdec, char postinc, predec

Fixtures `965` (`int g; x = g--;` — int global postdec as
value), `966` (`char g; x = g++;` — char global postinc as
value), `967` (`int g; x = --g;` — int global predec as
value).

965 and 967 already passed via batch 215's
`emit_update_to_ax` fast-path plus the postinc-deferred
peephole — `--` is just `inc` swapped for `dec` at every
site, no separate code needed.

966 needed a sibling peephole. The same ordering subtlety
from 963 applies to char globals: BCC emits

  mov al, byte ptr DGROUP:_g
  cbw                           ; widen captured byte
  mov word ptr [bp-2], ax       ; store to x
  inc byte ptr DGROUP:_g        ; mutate AFTER store

Whereas the generic `emit_update_to_ax` Post arm emits the
inc *before* the widen+store. Added a char-global Post
arm to the stack-local-assign peephole: load AL, widen
(cbw or `mov ah, 0` for uchar), store AX to local, then
deferred memory-direct inc/dec on the byte.

After this batch the four-shape grid is complete:

|              | Pre              | Post                  |
|--------------|------------------|-----------------------|
| int global   | inc + load (962) | load + store + inc (963) |
| char global  | load + inc + cbw (964) | load + widen + store + inc (966) |

The Post cases need the dedicated stack-assign peephole;
the Pre cases work through the generic
`emit_update_to_ax` because there's no use-vs-mutate
ordering question.

## Global `++` / `--` in expression context

Fixtures `962` (`int g; x = ++g;` — int global preinc as
value), `963` (`x = g++;` — int global postinc as value),
`964` (`char g; x = ++g;` — char global preinc as value).

All three previously panicked at `locals.rs:469` with
"unknown local in codegen: g" — `emit_update_to_ax` walked
the local-location-of path and assumed the target was a
local. Added a global-aware fast-path at the top:

- **Int/uint/pointer global, pre-update**: memory-direct
  `inc word ptr DGROUP:_g; mov ax, word ptr DGROUP:_g` —
  the side effect runs first, then the new value is loaded
  into AX. Captured value is the post-update one.
- **Int/uint/pointer global, post-update**: `mov ax, word
  ptr DGROUP:_g; inc word ptr DGROUP:_g` — capture pre-
  update value first, then mutate. (See ordering caveat
  below.)
- **Char/uchar global, pre-update**: AL detour: `mov al,
  byte ptr DGROUP:_g; inc al; mov byte ptr DGROUP:_g, al;
  cbw` (or `mov ah, 0` for uchar). Same shape as the
  stack-resident char case (fixture 732), but referencing
  DGROUP instead of `[bp-N]`.
- **Char/uchar global, post-update**: `mov al, byte ptr
  DGROUP:_g; inc byte ptr DGROUP:_g; cbw` — captured byte
  is pre-update.

963 also exposed a subtle ordering issue. BCC's actual
output for `x = g++` (x stack-resident) is:

  mov ax, word ptr DGROUP:_g    ; capture pre-update
  mov word ptr [bp-2], ax       ; store to x
  inc word ptr DGROUP:_g        ; mutate g AFTER the store

The mutation happens *after* the use. Our generic
`emit_update_to_ax` Post arm emits load+inc together,
placing the inc between the load and the caller's store.
Same instructions, different order — and BCC defers the
side effect past the using statement.

Added a peephole in the stack-local-assign path: when the
RHS is `Update { Post }` on an int/uint global, emit `mov
ax, _g; mov [target], ax; inc word ptr _g` directly,
deferring the side effect past the store. Sibling of the
existing `<stack-int> = <reg-int>++` peephole (fixture 649)
for register-resident locals.

## char `<<`, `-`, `~` as value

Fixtures `959` (`char c = 3; return c << 2;` — char left
shift by constant, returned as value), `960` (`char c = 5;
return -c;` — unary negation of a char), `961` (`char c =
0; return ~c;` — bitwise NOT of a char).

All three already work end-to-end:

- 959: char-shift-by-constant lowers the char to AX via
  `mov al, byte ptr [bp-1]; cbw`, then unrolls the shift
  into `shl ax, 1` repeated (count 2 → two `shl ax, 1`
  instructions, same shape as int 121). The post-widening
  result is int-sized, matching the integer promotion
  rule.
- 960: char unary minus mirrors the int 145/146 path —
  widen via `cbw`, then `neg ax` (`F7 D8`). The byte
  source produces a signed-extended int operand, so the
  negation is computed on the int.
- 961: char bitwise NOT is the analogous `~int` path —
  widen via `cbw`, then `not ax` (`F7 D0`). Same byte
  count as the unary-minus path; the only difference is
  the Group-3 sub-op (/2 for NOT vs /3 for NEG).

The common shape across 959/960/961 confirms that the
char-promotion-to-int rule is baked into every unary and
binary expression-position emit. No char-sized arithmetic
instructions are used in expression context — char
arithmetic that stays char-sized is restricted to compound
assigns where the destination is char-typed (fixtures
529, 666–674, etc.).

## char OR / XOR const, char `!` as value

Fixtures `956` (`char c = 15; return c | 4;` — char bitwise
OR with int constant, returned as value), `957` (`return c
^ 4;` — char XOR const), `958` (`char c = 0; return !c;` —
logical NOT of a char as a return value).

All three already work end-to-end:

- 956 / 957: the existing char-with-int-const arithmetic
  path widens the char via `cbw` (or `mov al, byte ptr
  [bp-1]; cbw`) and then emits `or ax, 4` / `xor ax, 4`
  against an int-typed AX. The 16-bit OR/XOR with a small
  constant uses the imm8sx Group-1 encoding (`83 /1 dd ii`
  for OR, `83 /6 dd ii` for XOR), one byte shorter than the
  imm16 form. Sibling of fixture 609 (char AND const).
- 958: `!c` for a char operand lowers exactly like `!int` —
  the operand is widened to AX, then the boolean-NOT
  materialization runs through the standard mini-CFG
  (`or ax, ax; jz .true; xor ax, ax; jmp .end; .true: mov
  ax, 1; .end:`). The widening is `mov al, byte ptr [bp-1];
  cbw`, and the rest is the same as fixture 618's `!x`
  path on int.

## char compare to int/char-literal as value, uint neg

Fixtures `953` (`char c = 5; return c < 10;` — char-vs-int
constant in return-value position), `954` (`char c = 'A';
return c == 'A';` — char-vs-char-literal in return value),
`955` (`unsigned a = 5; return -a;` — unary negation of an
unsigned value).

All three already work end-to-end:

- 953: existing `<char-stack-local, const>` arm in
  `emit_compare` handles this — `cmp byte ptr [bp-1], 10`
  (the int literal is truncated to 8 bits since the LHS is
  char-sized). Same `cmp byte ptr [bp+N], imm8` shape
  (`80 7E dd ii`) as fixture 524 used in if-condition
  position. Comparison-as-value lowering then materializes
  the boolean result through the standard six-instruction
  `cmp; jl .true; xor; jmp; .true: mov ax, 1` mini-CFG.
- 954: same byte-form `cmp byte ptr [bp-1], 65` (`'A'` is
  just the byte literal 65 — char literals are integer
  rvalues in C90). Sibling of 953 with a different
  comparison operator and a different RHS notation.
- 955: unary minus on an unsigned int promotes to the same
  signed-int `neg ax` (`F7 D8`) instruction. C90 says
  `-(unsigned)` evaluates as `(UINT_MAX + 1) - operand`,
  which on a wraparound two's-complement target is exactly
  what `neg` produces. BCC and our codegen both treat the
  operation identically to the signed case; no separate
  unsigned arm needed.

## uint compound `%=`, char-vs-char compare peephole

Fixtures `950` (`unsigned g; unsigned b; g %= b;` — uint
global compound mod-assign), `951` (`char c, d; return c ==
d;` — char-vs-char `==` as value), `952` (`return c < d;` —
char-vs-char `<` as value).

950 already passed via the batch-210 fix — the same arm at
emit_compound_assign:~4413 covers both `/=` and `%=`, and
the `gty.is_unsigned()` branch picks `xor dx, dx; div` for
both ops. Only the result-register selection differs (`ax`
for `/`, `dx` for `%`).

951 and 952 exposed a long-standing missed peephole. The
generic compare path was always emitting:

  mov al, byte ptr [bp-1]
  cbw                          ; widen to AX
  cmp ax, word ptr [bp-2]      ; word-sized compare

BCC instead emits a byte-byte compare with no widening:

  mov al, byte ptr [bp-1]
  cmp al, byte ptr [bp-2]      ; byte-byte compare

Two savings: one byte for the elided `cbw`, plus the
3-byte byte-form cmp (`3A 46 dd`) is one byte shorter than
the 4-byte word-form `cmp ax, [bp-N]` (`3B 46 dd`). Net
1-byte shrink per char-vs-char compare. Signed-vs-unsigned
character interpretation is encoded in the *jump* selection
(jl/jb), not in the cmp operand width — so the peephole is
safe across signedness combinations.

Implementation:

- Added a fast-path arm at the top of `emit_compare`,
  before the generic `emit_expr_to_ax(left)` fallthrough:
  when both LHS and RHS are char-typed stack-resident
  identifiers, emit `mov al, byte ptr <lhs>; cmp al, byte
  ptr <rhs>` and return early.
- New tasm IR variant `CmpAlBpRel { offset }` encoding the
  three-byte form `3A 46 dd`. Parser recognizes `cmp
  al,byte ptr [bp+N]` before the existing `cmp ax,…` arm.
- Currently restricted to stack-resident locals — a future
  fixture exercising char globals or char-register operands
  would extend the peephole symmetrically (or use a more
  general `CmpReg8Reg8`/`CmpReg8Mem8` shape).

## unsigned int mod, div-by-const, compound `/=`

Fixtures `947` (`unsigned a, b; return a % b;` — uint mod
with var RHS), `948` (`unsigned a; return a / 7;` — uint div
by constant), `949` (`unsigned g; unsigned b; g /= b;` — uint
global compound divide-assign with int-local RHS).

947 already passed end-to-end via the batch-209 fix — the
expression-context `BinOp::Div`/`Mod` arms in
`emit_op_with_source` route on `unsigned` and pick `xor dx,
dx; div` whenever the LHS expression is unsigned. The mod-
case is a free pass because the same widen-and-divide prefix
applies; only the result register differs (`dx` for `%`, `ax`
for `/`).

948 needed a new tasm IR variant — the immediate-divisor
path materializes the divisor in BX and then divides by BX
(register operand, not memory). `IdivReg16` already covered
the signed case; added `DivReg16 { reg }` for the unsigned
case. Encoding is `F7 (mod=11 /6 r/m=<reg>)` — same Group3
opcode as `IdivReg16`, just with /6 instead of /7. Parser
recognizes bare `div <reg>` after the `div al,byte ptr ...`
form has been ruled out.

949 needed a per-site codegen patch. The
`g <op>= local-RHS` path for div/mod with int-uint locals
(line ~4413, the "Int/uint global compound `/=` / `%=` with
an int/uint local RHS" arm) was hard-coded to `cwd; idiv`.
Added the same `unsigned`-flag branch we added in batch 209
to `emit_op_with_source` — when the LHS global is `UInt`,
pick `xor dx, dx; div` instead. There are several more
compound-assign sites with hardcoded `cwd; idiv` (lines
~4340 for the deref-pointer RHS, ~4471 for the char-RHS
widening dance, ~6383 for the long-pointer paths); future
fixtures that hit those paths will need the same fix.

## unsigned int add, mul, div — `xor dx, dx; div`

Fixtures `944` (`unsigned a = 5; unsigned b = 10; return a +
b;` — uint add), `945` (`return a * b;` — uint mul), `946`
(`return a / b;` — uint divide).

944 and 945 already worked end-to-end:

- 944: `add ax, [bp-N]` — same shape as signed int. The
  high-half overflow doesn't matter for 16-bit-wide results.
- 945: `imul word ptr [bp-N]` — BCC always uses `imul` here,
  *even for unsigned operands*. The unsigned `mul` instruction
  would only matter if we cared about the high half of a
  32-bit product — but `int × int → int` discards DX in both
  cases and `imul` and `mul` produce the same low-16-bit
  result. Saves the codegen one signed-vs-unsigned branch.

946 needed both an IR addition and a codegen change. BCC
distinguishes signed and unsigned for division because the
two instructions actually produce different results when AX
has its sign bit set:

- Signed (`int / int`): `cwd; idiv <r/m>` — sign-extends AX
  into DX:AX, then `idiv` treats DX:AX as a 32-bit signed
  dividend.
- Unsigned (`unsigned / unsigned`): `xor dx, dx; div <r/m>` —
  zero-extends, then `div` treats DX:AX as a 32-bit unsigned
  dividend.

For `unsigned a = 100; unsigned b = 7`, `cwd` would still
produce DX=0 (sign bit is clear) and `idiv` would still
return 14 — but the moment a value sets bit 15, the two paths
diverge. BCC always emits the matching pair.

Implementation:

- New `DivBpRel { offset }` IR variant in tasm. Encoding
  mirrors `IdivBpRel` but with ModR/M `0x76` (mod=01 /6=DIV
  r/m=110) instead of `0x7E` (/7=IDIV). Three bytes: `F7 76
  dd`. Parser recognizes `div word ptr [bp+N]` by routing
  through the same `parse_single_op_word_ptr` helper, after
  the explicit-AL byte form has been ruled out.
- Updated `emit_op_with_source`'s `Div` and `Mod` arms to
  pick `xor dx, dx; div` when the `unsigned` parameter is
  true (the same flag already used to pick `shr` over `sar`).
  Same selection for both the immediate-RHS path (via `bx`)
  and the memory-RHS path.

The other ~17 `cwd; idiv` sites in codegen aren't covered by
this change — they're for compound `/=` / `%=` and long
helpers, where the operand types are already constrained.
A future uint-compound-divide fixture will exercise those
sites; the per-site fix will follow the same pattern.

## int `--x`, `x--` as value, `char == -1`

Fixtures `941` (`x = 5; y = --x;` — int pre-decrement as
value), `942` (`x = 5; r = x--;` — int post-decrement as
value), `943` (`char c = -1; if (c == -1) return 1;` — char
compared against a negative literal).

All three already work end-to-end:

- 941: pre-decrement-as-value lowers to the same in-place
  decrement followed by an AX load, mirroring the
  pre-increment shape (fixture 530). BCC's pattern is `dec
  word ptr [bp-N]; mov ax, [bp-N]` — the decrement modifies
  the variable, and the AX load reads the *new* value as the
  expression's result.
- 942: post-decrement-as-value is the dual: `mov ax, [bp-N];
  dec word ptr [bp-N]` — AX captures the *old* value before
  the in-place decrement modifies the variable.
- 943: `c == -1` with `c` a signed char promotes the char to
  int via `cbw`, then compares with the int constant `-1`
  (`0xFFFF`). BCC's pattern: `mov al, byte ptr [bp-1]; cbw;
  cmp ax, -1`. The promotion is what makes the comparison work
  — without sign-extension, `0xFF == 0xFFFF` would fail.

## `||` as value, `^` as value, `>>` as value

Fixtures `938` (`int x = 1; int y = 2; return x || y;` —
logical OR as a return value, not an `if` condition), `939`
(`return x ^ y;` — bitwise XOR as value), `940`
(`int x = 64; int y = 2; return x >> y;` — arithmetic right
shift by a variable count as value).

All three already work end-to-end:

- 938: the `||`-as-value path was already producing the same
  three-block shape BCC emits — load left, short-circuit to
  `mov ax, 1` on true, fall through to test right, materialize
  `0` or `1` via the boolean-result mini-CFG. Same six-byte
  result-materialization as `==` / `<` / etc. but with two
  evaluation positions instead of one.
- 939: `mov ax, [bp-N]; xor ax, [bp-M]` — the generic
  reg-vs-stack `xor` emit path covers the rvalue position too,
  not just compound `^=`.
- 940: variable-RHS arithmetic right shift loads the shift
  count into CL (`mov cl, byte ptr [bp-M]`) and emits `sar ax,
  cl`. The byte load uses the low byte of the source word,
  which is correct for shift counts ≤ 31 (BCC doesn't mask).
  Same CL-prep path as compound shifts (fixture 658).

## `<` / `>` as value, bitwise OR as value

Fixtures `935` (`int x = 3; int y = 5; return x < y;` — `<`
comparison as a return value), `936` (`return x > y;` — `>`
as value), `937` (`int x = 0x12; int y = 0x34; return x | y;`
— bitwise OR as value).

All three already work end-to-end. The set fills out the
remaining int-comparison and integer-bitwise shapes that
hadn't yet been captured in OBJ form — `<=` and `>=` already
had OBJ fixtures (578 / 579) and `<=` as-value got added in
batch 205 (934). The signed-compare materialization is the
same six-instruction shape across `<` / `>` / `<=` / `>=`:
`cmp; jCC .true; xor ax, ax; jmp short .end; .true: mov ax,
1; .end:`, where `CC` matches the source operator (`jl` for
`<`, `jg` for `>`, etc.).

937 covers the third bitwise-as-value sibling — `&` was
already tested via `unsigned char` / `int` arms, `^` via the
ternary path, but the `|` rvalue had no direct OBJ
counterpart. BCC lowers `x | y` to `mov ax, [bp-N]; or ax,
[bp-M]` for stack-resident locals, which our generic
binary-op path already produces.

## Struct-array field rvalue, nested struct, `<=` as value

Fixtures `932` (`struct S { int n; int a[3]; } s; s.n = 7;
s.a[1] = 9; return s.n + s.a[1];` — global struct with an int
array field, used in an arithmetic rvalue), `933` (`struct A
{ struct B b; }; s.b.x = 42; return s.b.x;` — nested struct
member access via dot chain), `934` (`return x <= y;` — int
`<=` comparison used as a return value, not an `if` condition).

933 and 934 already worked end-to-end:

- 933: the existing member-chain helpers (`try_lvalue_chain_addr`
  / `try_member_dot_chain`) recurse through any number of Dot
  member nodes, accumulating field offsets. For `s.b.x` with
  both fields at offset 0, the chain resolves to
  `DGROUP:_s+0`, and the store/load fold into `mov word ptr
  DGROUP:_s, 42` / `mov ax, word ptr DGROUP:_s`.
- 934: the integer comparison-as-value path already handled
  `<=` via the same lowering as the `if`-condition path
  (`cmp; setle al; movzx ax, al`-equivalent on 8086:
  `cmp; jle .true; xor ax, ax; jmp .end; .true: mov ax, 1`).

932 needed one codegen fix in `OperandSource` resolution.
When a binary op had a member→array-index chain like `s.a[1]`
on its right-hand side, the existing `ExprKind::ArrayIndex`
arm walked the index list inline and panicked at the first
`Member` node it encountered ("array-index rhs: non-ident
base not supported"). Replaced the inline walk with a call to
`try_lvalue_chain_addr`, the same helper the `Member` rvalue
arm already used. That helper already recurses through
ArrayIndex *and* Member nodes uniformly — once the
ArrayIndex arm routes through it, mixed chains like
`s.a[K]`, `g.b.c[K]`, and `arr[i].field[j]` all fold to a
single `DGROUP:_<root>+<total_off>` operand. Cuts ~20 lines
of duplicate walk logic.

The arm still rejects non-global bases — local struct fields
through ArrayIndex would need a `[bp-N+K]` operand instead,
and no fixture exercises that path yet.

## Multi-var decl, `short`, `if` constant condition

Fixtures `929` (`int a, b; a = 3; b = 4; return a + b;` —
multi-variable declaration in a single statement), `930`
(`short s = 5;` — `short` keyword as a 16-bit int alias),
`931` (`if (1) { return 7; }` — literal-constant boolean
condition).

929 already works end-to-end — the parser's local-declaration
loop accepts a comma-separated declarator list and the locals
table allocates two distinct slots, both initialized by the
subsequent assigns.

930 needed one lexer change: the BC2.0 dialect accepts `short`
everywhere `int` does and produces the same 16-bit storage.
Rather than adding a separate `KwShort` token and threading it
through every type-parsing site (declarations, casts, sizeof,
function returns, struct fields, …), we map `short` directly
to `TokenKind::KwInt` in `lex_ident`. This collapses `short` /
`short int` / `unsigned short` into the existing `int` paths.
The downside is `short int s;` would lex as `int int s;` and
hit the dispatcher's "type at top level" failure — but no
current fixture pairs the two keywords. When one shows up, we
either add a dedicated `KwShort` or special-case the lexer's
buffer to skip a trailing `int` after `short`.

931 needed an `emit_if` fast-path. BCC constant-folds the
condition entirely: `if (1) { return 7; } return 0;` emits the
then-body inline (`mov ax, 7; jmp short @END`), then the
following statement (`xor ax, ax; jmp short @END`) with no
compare, no conditional jump, and no if-skip label between
them. The else-branch (if any) becomes dead code that BCC
emits anyway but never reaches. Implementation: at the top of
`emit_if`, run `try_const_eval(cond)`. If it folds, emit only
the relevant branch (then for non-zero, else for zero) and
skip the label-plan slot reservation entirely. The branch-skip
label that the conditional-jump path would emit is *not*
needed because there's no jump aimed at it — control simply
falls through to whatever comes next in the function body.

Same flavor as the existing `while (K)` fast-path further
down in this file: when a loop condition folds to a non-zero
constant, BCC elides the trampoline jump and the check label.
The `if (K)` shape is even simpler — no labels at all.

## Char init, void function, const variable

Fixtures `926` (`char c = 'A';` — char global with char-literal
init), `927` (`void set5(void) { g = 5; }` — void function
with explicit void params), `928` (`const int c = 5; return c;`
— const-qualified int global).

All three already work end-to-end. Coverage:

- 926: char literal `'A'` lowers to integer `65` masked to 8
  bits; `db 65` lands in `_DATA` under `_c`.
- 927: void return type plus explicit `void` parameter list —
  function body just sets globals, return path emits bare
  `ret` (no value materialization).
- 928: `const` qualifier on a global accepted at parser; codegen
  treats the global the same as a plain int. The qualifier
  doesn't change the OBJ — no read-only segment, no extra
  attributes. Modification through a writable lvalue would be
  UB but the codegen doesn't enforce it.

**Recorded findings (deferred):**

- **Array of pointers with non-const RHS** (`int *q[2]; q[0] =
  &a`): codegen panics "non-constant rhs in constant-indexed
  global array assign not yet supported" — the global-array
  store path's RHS handling doesn't yet accept address-of
  expressions for pointer-typed elements. Needs an arm in
  `emit_array_assign`'s global-array branch.
- **Main with command-line args** (`int main(int argc, char
  *argv[])`): same parser failure as fixture 922's array-as-
  parameter — `T name[]` in the parameter list panics "expected
  `)`, got `[`". Sized arrays (`char argv[16]`) likely also
  fail; the workaround in callers' code is `char **argv`.

## Array decay in call args, bitwise NOT, comma expr

Fixtures `923` (`int b[3]; f(b)` with `int *` param — array
decay), `924` (`return ~g` — bitwise NOT on global), `925`
(`i = (j = 5, j + 1)` — comma expression in rvalue position).

923 fixes the codegen bug recorded in batch 201: when an array
identifier is passed to a pointer parameter, the arg-prep path
emitted `mov ax, word ptr DGROUP:_b` (value load) instead of
`mov ax, offset DGROUP:_b` (address). Added an array-decay arm
in `emit_arg_into_ax` that checks the arg's type before
falling through to `emit_expr_to_ax`:

- Global array → `mov ax, offset DGROUP:_<name>` (3 bytes, no
  relocation needed for offset).
- Stack-local array → `lea ax, word ptr [bp-N]` (loads the
  effective bp-relative address into AX).

Both paths skip the value-load and produce the address
directly. Same array-decay rule applies as in C's "array name
in non-sizeof/non-address-of context becomes pointer to first
element" — the call site is exactly that context. Other
identifier types (non-array) still fall through to
`emit_expr_to_ax`.

924/925 already work end-to-end. Coverage:

- 924: `~g` emits `mov ax, word ptr DGROUP:_g; not ax`. The
  `not r/m16` form (Group3 /2) for AX is `F7 D0`.
- 925: comma operator in rvalue position evaluates the left
  subexpression for its side effects and discards the value,
  then evaluates the right subexpression and uses its value.
  Same lowering used in fixture 858 (compound RHS), now in
  plain rvalue context.

## do-while loops, while-global-cond

Fixtures `920` (do-while with accumulator: `do { s += i; i++;
} while (i < 5)`), `921` (basic do-while: `do { i++; } while
(i < 3)`), `922` (`int g; while (g) g = g - 1;` — while loop
with global zero-test condition).

All three already work end-to-end. Coverage notes:

- 920/921: do-while emits the body label at function entry,
  then the condition check at the bottom with a backward branch
  if true. No new IR — same shape as while-loop, just with the
  condition test moved to after the body.
- 922: while condition is a global zero-test. Reuses the
  existing `emit_zero_test` Ident-of-global arm (`cmp word ptr
  DGROUP:_g, 0`). The decrementing assignment `g = g - 1`
  lowers to `dec word ptr DGROUP:_g` (memory-direct INC/DEC
  peephole on int globals).

**Recorded findings from this batch (deferred):**

- **Function pointer assignment** (`int (*fp)(int) = f`): when
  RHS is a function symbol (not a local pointer), codegen
  panics with "unknown local in codegen: f". The assignment
  side needs an arm that recognizes the RHS as a function
  identifier and emits `mov word ptr <fp>, offset _f`.
- **Array-as-function-parameter** (`int f(int a[])`): parser
  fails at byte 11 with "expected `)`, got `[`" — the
  declarator grammar inside parameter lists doesn't yet
  accept the `T name[]` shorthand (must use `T *name`).
- **Array-decay-in-call-args** (`f(b)` where `b` is `int b[3]`
  and `f` expects `int *`): codegen emits `mov ax, word ptr
  DGROUP:_b` (value load) instead of `mov ax, offset DGROUP:_b`
  (address). The arg-prep path needs to detect array-typed
  args being passed to pointer params and emit the offset
  form.

## Enum values, function-static, union

Fixtures `917` (`enum E { A = 5, B = 10, C }; return C` —
enum with explicit values + auto-increment for `C`), `918`
(`int main() { static int g; ... }` — function-local static),
`919` (`union U { int i; char c[2]; }; union U u;` — basic
union with int/char overlay).

All three already work end-to-end. Coverage notes:

- 917: enumerator with explicit value sets the running counter
  (`A = 5`, `B = 10`); the next unspecified enumerator (`C`)
  auto-increments to `11`. The return-value path emits `mov
  ax, 11`.
- 918: `static` inside a function body promotes the local to a
  file-scope BSS symbol — but the symbol is *not* public.
  Codegen treats `g` like a private global (DGROUP-relative
  addressing), not a stack slot.
- 919: union layout — all members share the lowest offset
  (offset 0). `u.i = 0x4142` writes a word; `u.c[0]` reads the
  low byte (`0x42`, returned via `mov al, 0x42` widened to AX).
  Union shares the global's storage size = `max(member size) =
  2 bytes`.

## 2D array init, enum, typedef

Fixtures `914` (`int a[2][3] = {{1,2,3},{4,5,6}}` — 2D array
initializer), `915` (`enum E { A, B, C }; return B` — basic
enum), `916` (`typedef int Int; Int g` — typedef alias for int).

All three already work end-to-end. Coverage notes:

- 914: nested initializer list — outer braces group by row,
  inner braces fill each row's elements. The 6 ints land in
  `_DATA` row-major as `dw 1; dw 2; dw 3; dw 4; dw 5; dw 6`.
  `a[1][2]` reads at offset 5*2=10 from `_a`.
- 915: enum values are int-typed constants — the enumerator
  `B` materializes as the literal `1` in the return path
  (`mov ax, 1`). No enum-tag entry in the OBJ — the type info
  is purely parser-side.
- 916: `typedef int Int` registers `Int` in the parser's
  typedef table; `Int g` then parses identically to `int g`.
  No OBJ-level difference between the two.

## Struct/negative/pointer initializers

Fixtures `911` (`struct S { int x; int y; }; struct S s = {1,
2};` — struct initializer), `912` (`int g = -1;` — negative
int init), `913` (`char *p = "Hi";` — char pointer initialized
to string literal).

All three already work end-to-end. Coverage notes:

- 911: struct-shaped initializer list `{1, 2}` lands two `dw`
  entries (one per field) under the struct's symbol — same
  layout as a non-aggregate global, just stride-2 per int
  field.
- 912: `-1` lands as `0FFFFh` in `_DATA` — the sign-extension
  to 16 bits is handled by the same masking already in
  `try_const_eval`.
- 913: `char *p = "Hi"` emits the anonymous string constant in
  `_DATA` (`db 'H','i',0`) and a relocated word in `_DATA` for
  `_p` itself (pointing at the string's offset). The OBJ
  contains the FIXUPP record linking the pointer's bytes to
  the anonymous string's offset.

## String literal init, inferred array size, long init

Fixtures `908` (`char a[] = "Hi";` — string literal in char
array), `909` (`int a[] = {1, 2, 3};` — size-inferred array
init), `910` (`long g = 0x12345678L;` — long global init).

All three already work end-to-end. Coverage notes:

- 908: parser handles the C90 abbreviation `char a[] =
  "string"` — array size is `strlen("Hi") + 1 = 3`. Codegen
  lands the bytes into `_DATA` as `db 'H','i',0`.
- 909: same size-inference rule for `int a[] = {1, 2, 3};` —
  the explicit list determines the array's element count, and
  the (omitted) `[N]` in the declarator is filled in from the
  list length.
- 910: long initializer `0x12345678L` lands as a four-byte
  data record split into two `dw` lines, low half first
  (`dw 5678h; dw 1234h`) — same little-endian convention as
  long stores.

## Array/global initializers, static linkage

Fixtures `905` (`int a[3] = {1, 2, 3};` — array initializer
list), `906` (`int g = 42;` — int global with initializer),
`907` (`static int g;` — file-scope static).

All three already work end-to-end. The probes lock in byte-
exact regression coverage for parser-level shapes that had been
implemented in earlier batches but lacked explicit fixtures:

- Array initializer lists land entries in `_DATA` as a sequence
  of `dw K` lines under the symbol label (vs `_BSS` for
  uninitialized arrays). Parser handles the `{ K0, K1, ... }`
  shape inside `parse_initializer`.
- Single-int initializer (`int g = 42;`) puts `_g` in `_DATA`
  with a single `dw 42`.
- `static` storage class produces a non-public symbol in the
  OBJ — no `public _g` line, but the symbol is otherwise
  identical (`_g` in `_BSS`). The `LEDATA`/`LIDATA` placement
  and `_DATA`/`_BSS` segment selection don't change with
  `static`; only the publics directory does.

## Pointer subscript — long compound (OR, XOR, SHL)

Fixtures `902` (`long *p; p[1] |= 0xFL`), `903` (`long *p; p[1]
^= 0xFL`), `904` (`long *p; p[1] <<= 1`).

902/903 reuse the long-pointer subscript arm from batch 194:
the long-compound-to-mem helper already emits `or word ptr
[bx+lo], <lo>; or word ptr [bx+hi], <hi>` (and XOR sibling),
which TASM was already wired to encode via `OrBxDispImm16`/
`XorBxDispImm16` (batch 186).

904 exposed a new finding: BCC reloads BX between the inline
register-arith and the store-back for the K=1 long-shift form:

```
mov bx, _p
mov ax, [bx+6]
mov dx, [bx+4]
shl dx, 1
rcl ax, 1
mov bx, _p          ; reload — BCC doesn't keep BX live across shl/rcl
mov [bx+6], ax
mov [bx+4], dx
```

Same reload-after-arith pattern as `idiv` (batch 189 fixture 885)
and the char-pointer-AL-arith path (batch 182 fixture 865).
`emit_long_compound_to_mem` doesn't know the operand is BX-
relative or what symbol to reload, so the new long-pointer arm
in `emit_array_compound_assign` special-cases `Shl|Shr` with
`K=1` and emits the full sequence inline (load high/low into
AX/DX, inline shift, reload BX, store) rather than routing
through the helper. One new IR variant: `MovDxBxDisp { disp: i8 }`
(`8B 57 dd`) for the `mov dx, word ptr [bx+disp]` low-half load.

(Other helper-call paths in the same arm — shift K>1, mul,
div, mod — would also need BX reloads if exercised on this
shape; deferred until a probe demands them.)

## Pointer subscript — long compound (ADD, SUB, AND)

Fixtures `899` (`long *p; p[1] -= 5L`), `900` (`long *p; p[1]
&= 0xFL`), `901` (`long *p; p[1] += 5L`).

BCC's shape for any long compound on a global-pointer subscript:

```
mov bx, word ptr DGROUP:_p
<lo-op> word ptr [bx+off], <lo-imm>
<hi-op> word ptr [bx+off+2], <hi-imm>
```

Where `<lo-op>`/`<hi-op>` is one of the long-arith op pairs
(add/adc, sub/sbb, and/and, or/or, xor/xor) — same pairings as
the long-global compound path (fixtures 251/253/339). For
fixture 901's `+= 5L`: `add [bx+4], 5; adc [bx+6], 0`. For
899's `-= 5L`: `sub [bx+4], 5; sbb [bx+6], 0`. For 900's `&=
0xFL`: `and [bx+4], 0xF; and [bx+6], 0` (no carry — both halves
just AND independently).

Added a new arm in `emit_array_compound_assign` gated on
`gty.pointee().is_long_like()` + const single index. Emits `mov
bx, _p` once, then routes through the existing `emit_long_
compound_to_mem` helper with `[bx+off]` / `[bx+off+2]` as the
address pair. The helper already handles all long op families
(add/sub/and/or/xor and the shift compounds) — the new arm
just provides the BX-relative address pair to feed it.

Two new IR variants needed at the TASM layer for the carry/
borrow ops: `AdcBxDispImm8` (`83 57 dd ii` — Group-1 /2) and
`SbbBxDispImm8` (`83 5F dd ii` — Group-1 /3). The bitwise high
halves reuse `AndBxDispImm16` (etc., from batch 186). Other op
families (Mul/Div/Mod, shifts) defer through the helper too;
the helper's existing `N_LXLSH@` / `N_LDIV@` etc. helper-call
paths work unchanged since they don't address through BX
directly.

**Deferred from this batch (parser-aside):** non-const long
RHS for the assign form (`long *p; p[K] = x` where x is a
long lvalue) and the rvalue subscript-load (`long y; y =
p[K]`). Both need a `long_lvalue_addr_pair`-style helper that
emits a `mov bx, _p` prefix and returns BX-relative addresses
— the existing helper only returns plain memory addresses
since it's `&self`, not `&mut self`. Punted with the existing
"not yet supported" panic messages.

## Pointer subscript — char call arg, long assign, lt compare

Fixtures `896` (`char *p; f(p[1])` — char-pointer subscript as
int call arg), `897` (`long *p; p[1] = 42L` — long-pointer
subscript plain assign with const RHS), `898` (`int *p; if
(p[1] < g)` — pointer-subscript less-than compare against a
global).

896 already worked end-to-end. `emit_arg_into_ax` widens the
byte load to int via `cbw`/`mov ah,0` then pushes AX — same
shape BCC uses. 898 also already worked: it lowers through
the same `mov ax, [bx+disp]; cmp ax, word ptr DGROUP:_g`
sequence the AX-through compare path produces, which happens
to match BCC's actual OBJ bytes for this shape.

897 needed a long-pointee arm in `emit_array_assign`'s global-
pointer branch. BCC's shape:

```
mov bx, word ptr DGROUP:_p
mov word ptr [bx+6], <hi>    ; high half at offset+2
mov word ptr [bx+4], <lo>    ; low half at offset
```

Stride is 4 for long, so K=1 gives `[bx+4]` / `[bx+6]`. The
high-first store ordering matches the existing long-global and
long-array stores (batches around 302/322). Const RHS only —
non-const long RHS still panics ("non-constant rhs in `long
*p; p[K] = v` not yet supported"). New IR variant
`MovBxDispImm { disp: i8, imm: u16 }` (`C7 47 dd lo hi`, 5
bytes) — Group with `/0` (MOV r/m16,imm16), mod=01 r/m=111=BX.

## Pointer subscript — call arg, in arith, char rvalue

Fixtures `893` (`int *p; f(p[1])` — subscript as call arg),
`894` (`int *p; x = p[1] + 5` — subscript in arithmetic), `895`
(`char *p; return p[1]` — char-pointer subscript as return value).

894 and 895 already worked end-to-end without new code; the
rvalue subscript-load through `emit_expr_to_ax` handles the
arithmetic-binop and char-return paths.

893's BCC `-S` listing shows the memory-operand-push peephole
on the arg:

```
mov bx, word ptr DGROUP:_p
push word ptr [bx+2]
```

But the actual OBJ bytes are `mov ax, word ptr [bx+2]; push ax`
— **same `-S` vs OBJ discrepancy** as fixture 891. An early
attempt to apply the peephole in `emit_call` (and the matching
`PushBxDisp` IR variant + parser arm) emitted the listing-form
bytes (`FF 77 02`, 3 bytes) and ended up 1 byte shorter than
the oracle OBJ. Reverted the codegen — fall through to
`emit_arg_into_ax` + `push ax` and the bytes match. The
`PushBxDisp` IR variant (`FF 77 dd`) was left in place since
the encoding itself is correct; it just doesn't get exercised
by current fixtures.

Recorded broadly: BCC's `-S` printer over-eagerly substitutes
memory-direct forms (cmp, push) for the BX-indexed pointer-
subscript case, but the OBJ pipeline always routes through AX
for these. Compound-assign LHS (batches 181-189) and zero-test
(889) use the memory-direct forms in both listings *and* OBJ;
rvalue contexts (compare-with-const, push as call arg) only do
so in listings.

## Pointer subscript — return, compare-const, compare-var

Fixtures `890` (`int *p; return p[1]` — subscript as the return
value), `891` (`int *p; if (p[1] == 5)` — equality against a
const), `892` (`int *p; if (p[1] == q)` — equality against a
local var).

All three already worked end-to-end without new code — they
exercise the rvalue subscript-load path through `emit_expr_to_
ax` plus the existing comparison helpers. The probes lock in
byte-exact regression coverage for paths that previously had
no fixture witness.

**Recorded finding (BCC `-S` vs OBJ mismatch).** For fixture
891 the oracle's ASM listing reads `mov bx, _p; cmp word ptr
[bx+2], 5`, but the assembled OBJ bytes are actually `mov bx,
_p; mov ax, word ptr [bx+2]; cmp ax, 5`. BCC's `-S` printer
shows the memory-direct compare, but the internal OBJ pipeline
emits the AX-through form. An early attempt to add a memory-
direct cmp peephole to `emit_compare` matched the printed ASM
but produced a 2-byte-shorter OBJ than the oracle. Reverted —
falling through to `emit_expr_to_ax` + `cmp ax, imm` is what
the unchanged code already did, and it matches the OBJ bytes.
Fixture 889 (`if (p[K])`) is a real zero-test path through
`emit_zero_test`; that one *does* match both the ASM listing
and the OBJ bytes (`83 7F dd 00`, memory-direct).

## Pointer subscript — non-compound read/write/test

Fixtures `887` (`int *p; p[1] = y` — plain assignment to global
pointer subscript), `888` (`int *p; x = p[1]` — subscript as
rvalue), `889` (`int *p; if (p[1])` — subscript in boolean
context).

888 already worked end-to-end: the rvalue subscript-load
through a global pointer was already handled by an earlier
`emit_expr_to_ax` path. The fixture just locks in the byte
output.

887 needs a new arm in `emit_array_assign` for the global-
pointer base case. The function already chained through both
local pointers (fixture 590) and global arrays via
`try_const_array_offset`, but `globals.type_of(p)` returns a
`Pointer` (not `Array`), so the offset helper rejected it and
the function fell into the variable-index path that panics
("variable-indexed global array assign not yet supported").
Added a sibling arm gated on `gty.pointee()` + const single
index + int/uint pointee: load the pointer into BX, then `mov
word ptr [bx+K*2], <ax|imm>`. Same skeleton as the compound
path from batch 181 — uses the existing `MovBxDispAx` from
batch 188; var-RHS routes through `emit_expr_to_ax` first,
const-RHS emits the imm form directly.

889 needs both a codegen arm in `emit_zero_test` and a new IR
variant. BCC's shape for `if (p[K])`:

```
mov bx, word ptr DGROUP:_p
cmp word ptr [bx+K*2], 0
je @label
```

The `cmp` uses the imm8sx form (4 bytes `83 7F dd 00`) — same
preference as the flat global zero-tests. Added
`CmpBxDispImm8 { disp: i8, imm: i8 }` with ModR/M `7F` =
mod=01 reg=/7=CMP r/m=111=BX. The new `emit_zero_test` arm
fires when the condition is an ArrayIndex of a global pointer
with constant index.

## Pointer subscript — mod, div, char postinc

Fixtures `884` (`int *p; p[1] %= y` — mod compound), `885`
(`int *p; p[1] /= y` — div compound), `886` (`char *p; p[1]++`
— discarded char-pointer postinc).

884 reuses 883's mul/div/mod arm, but two things were missing:
- the **DX-result store** form (`mov word ptr [bx+disp], dx`)
  for the `%=` op — added IR variant `MovBxDispDx` (`89 57 dd`
  with ModR/M `57` = mod=01 reg=DX(010) r/m=111=BX).
- a **BX reload after idiv** before the store. `imul` (single-
  operand) doesn't clobber enough state to bother BCC, but
  `idiv` does, so BCC re-emits `mov bx, _p` between the `idiv`
  and the store. The codegen path now emits this reload on the
  Div/Mod branch only — `imul` keeps the existing tighter
  shape. Fixture 885 (div) needed this too; without the reload
  the OBJ differed by 3 bytes against the oracle.

886 needs the K=1 memory-direct peephole for char-pointee:
`inc|dec byte ptr [bx+K]` (3 bytes) instead of the 11-byte AL-
arith-through pattern. Added IR variants `IncBxDispByte` (`FE
47 dd`) and `DecBxDispByte` (`FE 4F dd`) — Group FE byte
counterparts to `IncBxDisp`. Codegen now detects `try_const_
eval(value) == 1` early in the char-pointer compound arm.

## Pointer subscript — postdec, variable shift, mul

Fixtures `881` (`int *p; p[1]--` — discarded postdec), `882`
(`int *p; p[1] <<= y` — variable shift), `883` (`int *p; p[1]
*= y` — multiplication compound).

881 already works end-to-end: the `K=1` Add/Sub → `inc|dec`
peephole + `DecBxDisp` IR variant landed in batch 187 covered
the postdec form too (postinc and postdec both reduce to the
same memory-direct `inc|dec word ptr [bx+K]` when the result
is discarded).

882 mirrors fixture 539's int-global variable shift, lifted to
BX addressing: `mov bx, _p; mov cl, byte ptr [bp-N]; <shift>
word ptr [bx+K*2], cl`. Three new IR variants `ShlBxDispCl`
(`D3 67 dd`), `SarBxDispCl` (`D3 7F dd`), `ShrBxDispCl` (`D3
6F dd`) — Group-2 variable-count shifts with mod=01 r/m=111=BX.
Codegen routes through `rhs_byte_addr` for the CL load, picks
SAR vs. SHR by signedness of the pointee.

883 mirrors the int-global Mul/Div/Mod path (fixture 802),
lifted to BX. BCC's shape:

```
mov bx, word ptr DGROUP:_p
mov ax, word ptr [bx+K*2]   ; load LHS
imul word ptr [bp-N]         ; multiply by stack RHS
mov word ptr [bx+K*2], ax    ; store result
```

(For Div: same but `cwd; idiv`. For Mod: result reads from
DX instead of AX.)

Two new MOV IR variants needed: `MovAxBxDisp` (`8B 47 dd` —
load through BX-disp8 into AX) and `MovBxDispAx` (`89 47 dd`
— store sibling). The single-operand `imul word ptr [bp+N]`
form already existed (`ImulBpRel`).

## Pointer subscript — shift, zero offset, postinc

Fixtures `878` (`int *p; p[1] <<= 3` — shift compound), `879`
(`int *p; p[0] += y` — zero-offset disp), `880` (`int *p;
p[1]++` — discarded postinc, the `K=1` add peephole).

879 needs the zero-disp form of the BX-based mem-direct ALU
ops — `add word ptr [bx], ax` etc. Added five `<op>BxPtrAx`
variants (`AddBxPtrAx`/`SubBxPtrAx`/`AndBxPtrAx`/`OrBxPtrAx`/
`XorBxPtrAx`) encoded as `01/29/21/09/31 07` (ModR/M `07` =
mod=00 reg=AX(000) r/m=111=BX). 2-byte form vs. the 3-byte
disp8 sibling — TASM picks the right encoding based on whether
the operand text is `word ptr [bx]` or `word ptr [bx+N]`.

880 needs `IncBxDisp { disp: i8 }` and `DecBxDisp` (`FF 47 dd`
for INC `/0`, `FF 4F dd` for DEC `/1`). The codegen-side
peephole was missing too: the global-pointer subscript path
emitted `add word ptr [bx+2], 1` (4 bytes via the imm8sx form)
instead of `inc word ptr [bx+2]` (3 bytes). Added the same
`v_masked == 1 && Add|Sub → inc|dec` peephole that fixture 547
exercises on the bp-relative array path.

878 needs a Shl/Shr/Sar arm in the global-pointer subscript
codegen — mirror of the int-global shift path (fixture 539):
load BX once, then unroll `<shift> word ptr [bx+K*2], 1` for
each bit of the (compile-time) shift count. Three new IR
variants `ShlBxDispImm1` (`D1 67 dd`), `SarBxDispImm1` (`D1 7F
dd`), `ShrBxDispImm1` (`D1 6F dd`) — all Group-2 1-bit shifts
with mod=01 r/m=111=BX (no `C1` imm8 form on 8086).

## Pointer subscript — const bitwise, negative index, char const

Fixtures `875` (`int *p; p[1] &= 15` — global int* const-RHS
bitwise), `876` (`int *p; p[-1] += y` — negative subscript via
`p = &a[2]`), `877` (`char *p; p[1] += 5` — char* const-RHS
ADD).

875 needs the imm16 const-RHS form for bitwise — BCC always
picks imm16 for AND/OR/XOR (no imm8sx peephole, same asymmetry
as the flat `g &= K` path that batch 76 first observed). Added
three new IR variants: `AndBxDispImm16` (`81 67 dd lo hi`),
`OrBxDispImm16` (`81 4F dd lo hi`), `XorBxDispImm16` (`81 77 dd
lo hi`) — all Group-1 with mod=01 r/m=111=BX+disp8. The codegen
side already emits `and word ptr [bx+2], 15` for any int op-
family with const RHS (the path landed in batch 182 / fixture
864 with imm form picked at the TASM layer); only TASM needed
new arms here.

876 needs no new code: `parse_word_bx_disp` already accepts
signed displacement, and codegen formats negative offsets as
`[bx-N]`. The probe confirms the i8 signed range works on both
sides of zero. (`p = &a[2]` lets `p[-1]` refer to a defined
array element, avoiding undefined behavior in the source.)

877 extends 865's char-pointee path to const RHS. The shape is
the same AL-arith-through with BX-reload-between-load-and-store,
just with `add al, 5` (the existing `AddAlImm8` 2-byte
accumulator form) instead of `add al, byte ptr [bp-N]`. The
gate now folds const and var paths through one `or_else` chain:
`try_const_eval(value).map(|v| (v & 0xFF).to_string()).
or_else(|| self.rhs_byte_addr(&value.kind))`.

## Pointer subscript — XOR and const-SUB coverage

Fixtures `872` (`int *p; p[1] ^= y` — int* XOR), `873`
(`char *p; p[1] ^= y` — char* XOR), `874` (`int *p; p[1] -= 5`
— const-RHS SUB for global int pointer).

No new code — all three exercise IR variants that were wired up
in earlier batches but lacked fixture coverage:

- 872 → `XorBxDispAx` (added with the Add/Sub/And/Or family in
  batch 181 / fixture 862).
- 873 → `XorBxDispAl` (added with `AndBxDispAl`/`OrBxDispAl` in
  batch 184 / fixture 870).
- 874 → `SubBxDispImm8` (added with `AddBxDispImm8` in batch 182
  / fixture 864).

These fill the XOR holes for both word- and byte-width pointer
subscript bitwise compound, and add explicit byte-exact
regression coverage for the const-RHS SUB form. The remaining
gaps in this family are bitwise-const variants (`p[K] &= 0xF`,
etc.) — BCC uses the imm16 encoding there even for small
constants, so they need a separate IR variant family.

## Char-pointer subscript — op-family expansion

Fixtures `869` (`char *p; p[1] -= y` — SUB sibling of 865),
`870` (`char *p; p[1] &= y` — AND), `871` (`char *p; p[1] |= y`
— OR).

869 needs no new code: the existing 865 path matches `Add|Sub`,
and BCC keeps the same AL-arith-through + BX-reload-between-
load-and-store shape for SUB.

870/871 expose the same op-family asymmetry that char-globals
and char-arrays already have (batches 121/122, 177): bitwise
compound stays *memory-direct* — no AL pre-load, no BX reload.
BCC's shape:

```
mov bx, word ptr DGROUP:_p
mov al, byte ptr [bp-N]   ; RHS into AL
and byte ptr [bx+K], al    ; mem-direct AND
```

Added a sibling arm gated on `pointee.is_char_like()` + `BitAnd
| BitOr | BitXor` + non-const byte RHS via `rhs_byte_addr`. New
IR variants `AndBxDispAl` (`20 47 dd`), `OrBxDispAl` (`08 47
dd`), `XorBxDispAl` (`30 47 dd`) cover the `<op> byte ptr [bx+
disp8], al` asm form (ModR/M `47` = mod=01 reg=AL(000) r/m=111=
BX+disp8). XOR is wired up but not yet fixture-covered.

The `mov al, byte ptr [bp-N]` step lands via the existing
`MovReg8BpRel` parser arm, and the `mov bx, word ptr DGROUP:_<p>`
goes through the existing global word-load — both unchanged.

## Pointer subscript compound — op-family siblings

Fixtures `866` (`int *p; p[1] -= y`), `867` (`int *p; p[1] &= y`),
`868` (`int *p; p[1] |= y`).

No new code — these exercise the `SubBxDispAx`/`AndBxDispAx`/
`OrBxDispAx` IR variants introduced alongside `AddBxDispAx` in
batch 181 (the same `<op> word ptr [bx+offset], ax` form for the
global-pointer subscript path). 862 only fixture-covered the ADD
op; these add explicit byte-exact regression coverage for SUB/AND/
OR siblings. XOR is also wired but waits on a fixture probe.

## Pointer subscript compound — local, const-RHS, char pointee

Fixtures `863` (`int *p; p[1] += y` — stack-local pointer in SI),
`864` (`int *p; p[1] += 5` — const RHS through a global pointer),
`865` (`char *p; p[1] += y` — char pointee).

863 picks up where 862 left off. BCC places stack-local
pointers in a register (typically SI/DI), and the subscript
compound becomes `<op> word ptr [si+K*2], ax`. Added a parallel
arm in `emit_array_compound_assign` gated on `self.locals.has(
array)` + `LocalLocation::Reg(reg)` + pointer pointee — same
RHS-into-AX prep as 862, but the memory operand is `[<reg>+
offset]` instead of `[bx+offset]`. New IR variants
`AddSiDispAx`/`SubSiDispAx`/`AndSiDispAx`/`OrSiDispAx`/
`XorSiDispAx` cover the asm form (encoded `01/29/21/09/31 44
dd` — ModR/M `44` = mod=01 reg=AX(000) r/m=100=SI). disp=0
stays with the existing 2-byte `AddSiPtrAx` family.

864 lifts the const-RHS gate from 862's path: when `try_const_
eval(value)` succeeds, emit `<op> word ptr [bx+offset], <K>`
directly instead of routing through AX. New IR variants
`AddBxDispImm8` / `SubBxDispImm8` encode the imm8sx form
(`83 47 dd ii` for ADD/0, `83 6F dd ii` for SUB/5). BCC picks
imm8sx when the constant fits a signed byte (just like the
flat `g += 5` path picks `83 06 lo hi 05` over the imm16 form);
AND/OR/XOR always use imm16 so they don't get the imm8sx
variant. The shared code now branches on `try_const_eval`:
const branch emits the imm form, var branch routes through
`emit_expr_to_ax`.

865 covers `char *p; p[K] += y` — BCC switches to the
AL-arith-through pattern (same op-family asymmetry as char-
global / char-array compound) and reloads BX between the load
and the store:

```
mov bx, word ptr DGROUP:_p
mov al, byte ptr [bx+K]
add al, byte ptr [bp-N]
mov bx, word ptr DGROUP:_p   ; reload — BCC doesn't keep BX live
mov byte ptr [bx+K], al
```

Added a sibling arm gated on `pointee.is_char_like()` + `Add/
Sub` + non-const byte RHS via `rhs_byte_addr`. New IR variants
`MovAlBxDisp` (`8A 47 dd`) and `MovBxDispAl` (`88 47 dd`) for
the load/store at `[bx+disp8]`; disp=0 stays with the existing
`MovAlFromBxPtr` 2-byte form. Char-pointee bitwise (`&=`/`|=`/
`^=`) and signed/unsigned distinctions are still deferred —
each requires a separate probe to characterize the exact shape.

## Compound LHS with non-Ident base

Fixtures `860` (`a[1].x += y` — global struct-array element
member), `861` (`o.inner.x += y` — nested dot chain through a
global outer struct), `862` (`p[1] += y` — global int pointer
subscripted by constant).

860 and 861 already worked end-to-end: `try_member_dot_chain`
already chains through `ArrayIndex` and nested `Member` bases
via `try_lvalue_chain_addr`, so the resulting `(name, total_off,
leaf_ty)` resolves to the same `DGROUP:_<name>+<off>` form the
flat `s.x += y` path uses, and `emit_member_compound_assign`
emits the same `mov ax, <rhs>; add word ptr DGROUP:_..., ax`
shape that fixture `832` exercises. These probes verify the
chain-folder doesn't drop offsets when the base is an
ArrayIndex (line `_a+2` for `a[1].x`) or another Member
(`_o+0` for `o.inner.x`, since `inner` is at offset 0 inside
`Outer`).

862 needed new code. BCC's shape for `int *p; p[K] += y`:

```
mov bx, word ptr DGROUP:_p
mov ax, word ptr [bp-2]   ; emit_expr_to_ax for y
add word ptr [bx+K*2], ax
```

`emit_array_compound_assign` only had paths for **array**-typed
globals — when `array` is a global int *pointer*, the function
fell through to `self.locals.type_of(array)` and panicked
("unknown local in codegen: p"). Added a new guarded path:

- `self.globals.type_of(array)` is `Some(Pointer(pointee))`,
- single constant index → compile-time offset
  (`K * pointee.size_bytes()`),
- int pointee, `Add/Sub/BitAnd/BitOr/BitXor`, non-constant RHS.

Emits `mov bx, word ptr DGROUP:_<p>` then routes through
`emit_expr_to_ax` (which handles char/uchar widening too) and
finishes with `<op> word ptr [bx+offset], ax`.

The asm-level `<op> word ptr [bx+disp8], ax` form wasn't a
recognized IR shape yet. Added five new variants —
`AddBxDispAx`/`SubBxDispAx`/`AndBxDispAx`/`OrBxDispAx`/
`XorBxDispAx`, all with `disp: i8`. Encoding is
`<opcode> 47 dd` for ADD/SUB/AND/OR/XOR (`01/29/21/09/31`)
where ModR/M byte `47` decodes as mod=01 reg=AX(000)
r/m=111=BX+disp8. A new parser helper `parse_word_bx_disp`
recognizes `word ptr [bx]` and `word ptr [bx+K]`/`[bx-K]`;
the per-op parse arms restrict the new variants to `disp != 0`
so a future `AddBxPtrAx` (disp=0, encoded `01 07`, 2 bytes
vs. 3) can claim the zero-disp form when a fixture eventually
exercises it.

Char/uchar/long pointees and the Mul/Div/Mod/Shl/Shr op
families are still deferred — same panic site, just with
fixture coverage missing.

## `int g += (int)c` / `+= comma` / `+= (y=5)`

Fixtures `857` (`g += (int)c` cast), `858` (`g += (a,b,c)`
comma), `859` (`g += (y=5)` assign expression).

Three more RHS shapes for `rhs_int_compound_type`:
- `ExprKind::Cast` — target type determines result; accept
  any int-family target.
- `ExprKind::Comma` — recurse into the right (last)
  subexpression's type. emit_expr_to_ax evaluates each
  subexpression for side effects and leaves the last in AX.
- `ExprKind::AssignExpr` — look up the target's type via
  globals/locals. emit_expr_to_ax stores the value and
  leaves it in AX.

Note `y++` as RHS (post-increment) was tried and deferred:
BCC has a peephole that uses the RHS register directly
(`add word ptr <g>, si; inc si`) rather than routing
through AX. Requires a separate dispatch arm.

## `int g += f()` / `+= ?:` / `+= !y` (call / ternary / not)

Fixtures `854` (`g += f()` call result), `855` (`g += y ? 1 : 2`
ternary), `856` (`g += !y` logical not).

Extended `rhs_int_compound_type` to handle:
- `ExprKind::Call` — assume int return (most common; long-
  returning calls would need a separate path).
- `ExprKind::Logical` — `!y`, `a && b`, `a || b` always
  yield 0/1 in AX, int-typed.
- `ExprKind::Ternary` — recurses into both branches; if
  both resolve to non-long int-family, result is int.

emit_expr_to_ax handles each form already (call → CALL,
logical → conditional branch into 0/1, ternary → if-
else pattern). The same memory-direct `add word ptr
<g>, ax` finishes for all three.

## `int g += -y` / `+= (y+1)` / `+= y*2` (expr RHS)

Fixtures `851` (`g += -y` unary neg), `852` (`g += (y+1)`
sub-expression), `853` (`g += y * 2`).

- `851` — extended `rhs_int_compound_type` to recurse
  into `ExprKind::Unary` (returning the operand's
  type). `emit_expr_to_ax` already materializes the
  negation in AX, then the existing memory-direct
  `add word ptr <g>, ax` finishes.
- `852` — extended `rhs_int_compound_type` to handle
  `ExprKind::BinOp` with both operands int-typed (and
  neither long-typed — long sub-expressions don't fit
  in AX). `emit_expr_to_ax` computes the sub-expr
  result in AX, then the mem-direct add finishes.
- `853` — free pass via the same BinOp path. The
  `y * 2` sub-expr resolves to int, AX gets the
  multiply result, mem-direct add finishes.

The helper now also recognizes `IntLit` (constants) —
mostly for completeness; the const-folded paths take
precedence in the dispatch chain.

## char member/array compound; arrow long member ADD

Fixtures `848` (`s.c += y` char member), `849` (`p->l += y`
arrow long member), `850` (`a[1] &= y` char array bitwise).

- `848` — char member compound with int RHS uses the AL-
  through pattern (same as fixture 847 char-array
  arith). Existing char-field path was gated on char-
  typed RHS only (mem-direct, fixture 708). Split into
  two paths now: char RHS keeps mem-direct, int RHS
  uses AL-through.
- `849` — long pointee compound `*p += int x` (here
  `p->l` which lowers to `(*p).l` with the pointer in
  SI). `emit_long_compound_to_mem` widens the int via
  `cwd` and emits `add word ptr [si], ax / adc word
  ptr [si+2], dx`. New IR variants `AdcSiDispDx` (`11
  54 dd`) and `SbbSiDispDx` (`19 54 dd`) for the high-
  half carry/borrow with DX (existing `AdcSiDispAx`
  was AX-only, used by long-long add).
- `850` — char array `&=` int var: BCC keeps the bitwise
  ops memory-direct rather than going through AL (the
  same asymmetry as char-global compound, batch
  121/122). Split the char-array Add/Sub/Bit* path into
  two: arith uses AL-through, bitwise uses mem-direct.

Also extended `emit_long_compound_to_mem` (member/array
long compound) to accept the int-RHS widening case —
opens up long member/array `+=` int var across both
dot and arrow forms.

## long member/array += int var; char array += int var

Fixtures `845` (`s.l += y` long member), `846` (`la[1] += y`
long array), `847` (`a[1] += y` char array, int RHS).

- `845` — long member compound with int var RHS:
  added `Type::Int|Type::Char` and `Type::UInt|Type::UChar`
  widening paths in `emit_long_compound_to_mem`. Same
  cwd/zero-extend logic as the long-LHS arms for global
  destinations (fixture 755, 767), but with the destination
  addresses passed in as opaque `lo_addr`/`hi_addr` strings
  (works for struct field, array element, etc.).
- `846` — free pass via batch 175 long-array path (the
  array element gets routed through `emit_long_compound_to_mem`
  with the new int-widening path).
- `847` — char array compound with int var RHS truncated
  to byte: `mov al, byte ptr <dest>; add al, byte ptr
  <rhs>; mov byte ptr <dest>, al`. Five new AL/byte-bp IR
  variants (`AddAlBpRel`, `SubAlBpRel`, `AndAlBpRel`,
  `OrAlBpRel`, `XorAlBpRel` — `02|2A|22|0A|32 46 dd`).
  These are AL-specific forms of `<op> r8, r/m8` that BCC
  uses when truncating an int local to a byte for char-
  compound destinations.

## `p->x += y`, `p->x *= y`, `p->x <<= y` (arrow member)

Fixtures `842` / `843` / `844` — three free passes
confirming the int-field compound paths added in
batches 171 and 172 generalize from `.` (Dot) to `->`
(Arrow) member access:

- The arm builds `dest` as `[<reg>]` (or `[<reg>+off]`
  if field offset is non-zero) for arrow form, vs
  `DGROUP:_<name>+<off>` for dot form. The Add/Sub/Bit*,
  Mul/Div/Mod, and Shift paths use `dest` as opaque
  text, so both addressing modes work without special-
  casing.
- `843`'s `imul word ptr [bp+N]` and `844`'s `shl word
  ptr [si], cl` had previously been added for non-arrow
  fixtures (834, 835) — they only depend on the dest
  string format.

No code changes — confirms the arrow member compound
inherits everything from dot member compound.

## `*p *= y`, `*p <<= y`, `*p &= y`

Fixtures `839` (`*p *= y`), `840` (`*p <<= y`),
`841` (`*p &= y`).

- `839` — int-deref Mul: `mov ax, word ptr [si]; imul
  word ptr [bp+N]; mov word ptr [si], ax`. Mirrors
  fixture 836 (array Mul) with `[si]` instead of an
  address. Reuses existing `MovAxSiPtr`, `ImulBpRel`,
  and `MovSiPtrAx` IR. Codegen-only.
- `840` — int-deref Shift: `mov cl, byte ptr <rhs>;
  shl/sar/shr word ptr [si], cl`. New IR variants
  `ShlSiPtrCl` / `SarSiPtrCl` / `ShrSiPtrCl` (D3 24 /
  D3 3C / D3 2C — Grp2 /4|/7|/5 r/m=100). Sibling of
  fixture 837 with the `[si]` form.
- `841` — `*p &= y` free pass via batch 173's
  `AndSiPtrAx` arm.

## `a[K] *= y`, `a[K] <<= y`, `*p += y`

Fixtures `836` (`a[1] *= y`), `837` (`a[1] <<= y`),
`838` (`*p += y`).

- `836` — array element Mul with non-const int local
  RHS: `mov ax, <dest>; imul word ptr [bp+N]; mov
  <dest>, ax`. Mirrors fixture 834 (member compound
  Mul), just with the array-element address. Added to
  `emit_array_compound_assign` alongside the existing
  Add/Sub/Bit* var-RHS path.
- `837` — array element Shift with non-const RHS:
  `mov cl, byte ptr <rhs>; shl word ptr <dest>, cl`.
  Reuses `rhs_byte_addr` (batch 169). Sibling of
  fixture 835.
- `838` — `*p += y` (int pointee, non-const RHS):
  `emit_expr_to_ax(value); add word ptr [si], ax`.
  New IR variants `AddSiPtrAx`, `SubSiPtrAx`,
  `AndSiPtrAx`, `OrSiPtrAx`, `XorSiPtrAx` for the
  `<op> r/m16, ax` form against `[si]` (encodings
  `01|29|21|09|31` followed by `04`). Codegen arm
  gated on pointer being register-resident with int
  pointee.

## `a[K] += y`, `s.x *= y`, `s.x <<= y` (non-const RHS)

Fixtures `833` (`a[1] += y`), `834` (`s.x *= y`),
`835` (`s.x <<= y`).

- `833` — int-array-element compound with non-constant
  RHS. `emit_array_compound_assign` previously panicked
  in this case. Added a path that mirrors the int-global
  Add/Sub/Bit* arm: `emit_expr_to_ax` produces AX from
  the RHS (with any widening), then `<op> word ptr
  <dest>, ax` writes back. `dest` already has the
  constant index folded as `DGROUP:_a+<K*stride>`.
- `834` — int-member compound `*=` with non-constant
  local RHS. Added a path in `emit_member_compound_assign`
  using `imul word ptr [bp+N]` directly against the
  member address. Same shape as fixture 802 with the
  member's effective address. Same path handles `/=`
  and `%=` (selecting AX or DX for the store).
- `835` — int-member compound `<<=` / `>>=`. Reuses the
  `rhs_byte_addr` helper (batch 169) to load CL from
  the RHS, then `shl/sar/shr word ptr <dest>, cl`.

Three new paths in member/array compound; no new IR
required — all shapes already encodable via the
existing imul/idiv/shl word ptr forms.

## `long` `*=` long-array; `s.x += y` int-member compound

Fixtures `830` (`g += la[1]`), `831` (`g *= la[0]`),
`832` (`s.x += y`).

- `830` — free pass via batch 170's long-RHS Add arm
  with non-zero stride offset (`_la+4` for index 1 of
  a long array).
- `831` — extended the new long-RHS arm to cover `Mul`
  (and `Div`/`Mod` for completeness). Same call-helper
  shape as `long_global *= long_global` (fixture 260):
  `mov cx, <rhs_hi>; mov bx, <rhs_lo>; mov dx, <lhs_hi>;
  mov ax, <lhs_lo>; call N_LXMUL@; store`. With array
  RHS, only the address strings differ.
- `832` — `s.x += y` (int field, non-const RHS): added
  a new path in `emit_member_compound_assign` for non-
  byte int fields with non-constant RHS. Pattern is
  the same as int-global compound add (`emit_expr_to_ax;
  <op> word ptr <dest>, ax`) — `dest` already includes
  any field offset folded into the struct address.
  Previously this case panicked (`non-constant rhs in
  member compound assign not yet supported`).

## `long` global compound `+=` with array / member / long-array RHS

Fixtures `827` (`g += a[1]` int array), `828` (`g += s.x`
int member), `829` (`g += la[0]` long array).

- `827` / `828` — extending the long-LHS Int/Char and
  UInt/UChar widening arms to use the broader
  `rhs_int_compound_type` helper (which resolves
  ArrayIndex and Member in addition to Ident). The
  widening logic (`cwd` for signed, `<hi_op> 0` for
  unsigned) is unchanged.
- `829` — new long-RHS variant accepting non-Ident RHS.
  `long_rhs_halves` returns (low, high) DGROUP addresses
  for ArrayIndex (const index, long element) and Member
  (long field). Same emission shape as `long_global +=
  long_global` (fixture 734) but with the array/member
  addresses substituted.

Also: this batch revealed a publics-ordering rule gap.
BCC reverts to reverse-alpha for the long bucket when
**any** global is long-typed (or wraps a long), even if
short and long globals coexist (which normally
triggered forward-alpha). Added `Type::contains_long()`
and `has_long_typed_global` check in `emit_s.rs`.
Pinned by fixture 829 (`long g; long la[3]; int main`)
which expects `_main, _la, _g`; the prior rule emitted
`_la, _main, _g`. Verified no regression across all
existing long-global fixtures.

## `int` global compound `*=` / `/=` / `<<=` with array / deref / member RHS

Fixtures `824` (`g *= a[1]`), `825` (`g /= *p`),
`826` (`g <<= s.x`) — extending the Mul/Div/Shift arms
to accept array / deref / member RHS forms:

- `824` — `imul word ptr DGROUP:_a+2`: existing
  `ImulGroupSym` encoder, but the arm now constructs the
  address from a constant-indexed array via the new
  `global_int_rhs_addr` helper.
- `825` — `idiv word ptr [si]` for `*p` where `p` is
  register-resident. New IR variants `ImulSiPtr` (F7 2C)
  and `IdivSiPtr` (F7 3C) for the deref-through-SI
  form. Codegen arm gated on register-resident int*
  pointer.
- `826` — `mov cl, byte ptr DGROUP:_s; shl word ptr
  DGROUP:_g, cl`. The shift arm now uses a new
  `rhs_byte_addr` helper that resolves the byte-pointer
  form for any of Ident / ArrayIndex / Member RHS — and
  for stack-resident bases — without needing per-form
  branches.

Two new helpers (`global_int_rhs_addr`,
`rhs_byte_addr`) plus two new IR variants
(`ImulSiPtr`, `IdivSiPtr`).

## `int` global compound `+=` with array / deref / member RHS

Fixtures `821` (`g += a[1]`), `822` (`g += *p`),
`823` (`g += s.x`) — extending the int-global Add/Sub/
Bit* arm to accept non-Ident RHS shapes:

- `821` — `a[1]` (constant array index): emits `mov ax,
  word ptr DGROUP:_a+2; add word ptr DGROUP:_g, ax`.
  emit_expr_to_ax already folds the constant index into
  the address offset.
- `822` — `*p` (deref of register-resident pointer):
  emits `mov ax, word ptr [si]; add word ptr DGROUP:_g,
  ax`. emit_expr_to_ax handles the deref of a SI-bound
  int pointer.
- `823` — `s.x` (global struct member): emits `mov ax,
  word ptr DGROUP:_s; add word ptr DGROUP:_g, ax`. The
  member offset folds into the symbol+offset form.

Added a new helper `rhs_int_compound_type` that
resolves the result type for `ArrayIndex`, `Deref`, and
`Member` in addition to plain `Ident`. The Add/Sub/Bit*
arm now uses this broader helper, dropping the
`ExprKind::Ident` gate. All three patterns produce the
same memory-direct `<op> word ptr DGROUP:_<g>, ax`
shape, so no new IR or encoding was needed.

## `long` stack-LHS compound `+=` / `*=` with byte var

Fixtures `818` (`a += char c`), `819` (`a += uchar c`),
`820` (`a *= char c`) — three free passes confirming the
long-LHS byte-RHS arms (fixtures 783, 784, 785) work
identically with a stack-resident long.

`long_halves_of` resolves to `[bp+off]` and `[bp+off+2]`
for a stack long, so:

- `818` / `819` — Add arm (signed/unsigned widening)
  emits `cbw / mov ah, 0; cwd / -; add word ptr [bp+lo],
  ax; adc word ptr [bp+hi], dx/0`. The widening logic
  and op selection are unchanged from the global-LHS
  version.
- `820` — Mul arm (signed `cbw + cwd + push/pop dance`)
  also writes back via `mov word ptr [bp+lo], ax; mov
  word ptr [bp+hi], dx`.

The "widening shape from RHS, addr form from LHS"
split confirmed again across stack/global LHS.

## `int` global compound `*=` / `/=` with byte-global RHS

Fixtures `815` (`g *= char c`), `816` (`g /= char c`),
`817` (`g *= uchar c`) — extending the byte-RHS `*=` /
`/=` arms (fixtures 796, 798) from local-only to also
accept global RHS:

- `815` — `emit_expr_to_ax` reads the char global via
  `mov al, byte ptr DGROUP:_c; cbw`, then the same
  push/pop shuffle (`push ax; mov ax, <lhs>; pop dx;
  imul dx`) finishes. No new IR or encoding — the byte-
  global load was already supported.
- `816` — same arm with `cwd` and `idiv bx` shuffle for
  divide.
- `817` — uchar RHS uses `mov ah, 0` rather than `cbw`,
  but the same push/pop dance against AX/DX/BX. Signed
  `imul` produces the correct low-16 product.

Code change: dropped `!self.globals.contains(b)` from
the two byte-RHS Mul/Div arms. The arms already used
`emit_expr_to_ax` (which is global-aware), so the
restriction was purely arbitrary scoping.

## `int` global compound `+=` char-global, `%=` global

Fixtures `812` (`g += char_global c`), `813` (`g += uchar_global c`),
`814` (`g %= int_global h`).

- `812` — `int g += char c` where both are globals.
  `emit_expr_to_ax` reads the char global via `mov al,
  byte ptr DGROUP:_c; cbw`, then the existing mem-direct
  `add word ptr DGROUP:_g, ax` shape finishes. Relaxed
  the Add/Sub/Bit* arm's gate from "local RHS only" to
  any RHS — the same generation works for char/uchar
  globals and supersedes fixture 571's narrower Int+
  Int-global arm at the same output bytes.
- `813` — free pass via the same arm. `emit_expr_to_ax`
  produces `mov al, ...; mov ah, 0` for the uchar zero-
  extension.
- `814` — free pass off batch 164's Mul/Div/Mod arm
  which already gated `BinOp::Mod` and selects `dx` for
  the store. Confirms `%=` works with global RHS.

The old `Int+Int-global` Add/Sub arm at fixture 571
remains in source (still fires first in source order)
but is now redundant — same emitted bytes. Left in
place for now since removing wouldn't change behavior.

## `int` global compound `*=` / `/=` / `<<=` with global RHS

Fixtures `809` (`g *= h`), `810` (`g /= h`), `811` (`g <<= h`)
— int compound with another int **global** as the RHS.

- `809` — `imul word ptr DGROUP:_h` directly against
  memory. New IR variant `ImulGroupSym` (F7 2E lo hi:
  Grp3 /5 with mod=00 r/m=110). The codegen arm fires
  on `int-global LHS + int-global RHS + Mul|Div|Mod`,
  parallel to fixture 802's local-RHS path but using
  the new DGROUP-form encoder.
- `810` — `/= h`: same arm with `cwd; idiv word ptr
  DGROUP:_h`. New IR variant `IdivGroupSym` (F7 3E lo
  hi). The push/cwd/pop dance the byte-RHS path needs
  is avoided here — neither AX nor DX has competing
  duties since `idiv` consumes both for the dividend
  and the global is read directly from memory.
- `811` — `<<= h`: extends batch 162's `Shl|Sar|Shr
  GroupSymCl` arm to read CL from a global instead of
  `[bp+N]`. The `mov cl, byte ptr DGROUP:_h` form was
  already supported by the existing
  `parse_byte_group_symbol` path in the parser; only
  the codegen arm needed to drop its `!globals.contains
  (b)` restriction.

## `int` / `uint` global compound shift siblings

Fixtures `806` (`int g <<= char c`), `807` (`int g >>= int x`),
`808` (`uint g >>= int x`) — three free passes confirming
batch 162's new memory-direct CL-shift arm generalizes:

- `806` — Char RHS uses the same `mov cl, byte ptr <addr>`
  load (CL only needs the low byte regardless of RHS
  width). The arm's RHS-type gate already accepted
  `Type::Char | Type::UChar`.
- `807` — `>>=` on signed int picks `sar` (D3 3E)
  rather than `shl`, via the existing signedness check
  on the LHS type.
- `808` — `>>=` on unsigned int picks `shr` (D3 2E).
  Same arm, just `gty.is_unsigned()` flips the mnemonic.

The `Shl|Sar|ShrGroupSymCl` IR variants added in batch
162 cover all three operations and both signednesses
via the encoding-byte selector. No code changes for
this batch.

## `int` global compound `/=`, `%=`, `<<=` with int var

Fixtures `803` (`g /= x`), `804` (`g %= x`), `805` (`g <<= x`)
— int-LHS / int-local-RHS variants.

- `803` — int `/= int`: memory-direct `idiv word ptr
  [bp+N]`. No widening needed since both operands are
  16-bit, no register-shuffle since `idiv` consumes
  DX:AX and a mem operand directly:

  ```
  mov ax, word ptr DGROUP:_<g>
  cwd                              ; DX:AX = sign-ext g
  idiv word ptr [bp-N]             ; AX = quot, DX = rem
  mov word ptr DGROUP:_<g>, ax     ; (or `, dx` for %=)
  ```

  Added a new arm in `emit_compound_assign`. Reuses the
  existing `IdivBpRel` IR variant — codegen-only.
- `804` — free pass via the same arm: `%=` selects `dx`
  for the store.
- `805` — int `<<= int`: BCC loads the shift count into
  CL from a `byte ptr [bp+N]` view, then shifts the int
  global memory-direct via `shl word ptr DGROUP:_g, cl`.
  The word-form `shl/sar/shr <mem>, cl` (D3 /4|/7|/5)
  hadn't been used before — only the byte-form (D2 ...,
  fixture 697). Added three new IR variants
  `Shl|Sar|ShrGroupSymCl` with `D3 26/3E/2E lo hi`
  encoding and parser entries for the `shl word ptr
  DGROUP:_g, cl` syntax. Codegen arm gates on
  `Type::Int | Type::UInt | Type::Char | Type::UChar`
  RHS — CL only needs the low byte regardless of RHS
  width.

## `int` global compound `/=` uchar, `%=` char, `*=` int

Fixtures `800` (`g /= uchar c`), `801` (`g %= char c`),
`802` (`g *= int x`) — fills out the int-compound dispatch.

- `800` — free pass off batch 160's `Type::Char|Type::UChar
  + Div|Mod` arm. The signed `idiv bx` correctly handles
  zero-extended uchar divisor (always positive).
- `801` — free pass via the same arm: `%=` differs only
  in which register the helper stores back (`dx` vs `ax`),
  which the arm already selects from the op variant.
- `802` — int `*= int x`: no widening needed since both
  operands are 16-bit, so BCC uses the single-operand
  `imul word ptr [bp+N]` form (F7 6E dd) directly against
  memory:

  ```
  mov ax, word ptr DGROUP:_<g>
  imul word ptr [bp-N]         ; DX:AX = AX * mem
  mov word ptr DGROUP:_<g>, ax ; low-16 stored
  ```

  Added a narrow arm in `emit_compound_assign` gated on
  `int LHS + Type::Int|Type::UInt RHS + BinOp::Mul +
  stack-local RHS`. Reuses the existing `ImulBpRel` IR
  variant — codegen-only change. The byte-RHS Mul arm
  (fixture 796) handles the push/pop shuffle case
  separately.

## `int` global compound `*=` uchar, `/=` char, `+=` int

Fixtures `797` (`g *= uchar c`), `798` (`g /= char c`),
`799` (`g += int x`).

- `797` — free pass off batch 159's int-compound Mul arm
  which already gated on `Type::Char | Type::UChar`. The
  signed `imul dx` produces the correct low-16 result
  for any operand combination.
- `798` — new shape for int `/= byte`: BCC reuses the
  push/pop register-shuffle pattern but parks the
  widened RHS in BX rather than DX (Div uses BX by
  convention; Mul used DX). The LHS load needs both AX
  and DX (the latter populated by `cwd`), so the push/
  pop bracket has to fence both the AX load and the
  cwd:

  ```
  mov al, byte ptr <c>
  cbw                          ; AX = char as int
  push ax                      ; save widened RHS
  mov ax, word ptr DGROUP:_<g> ; AX = g
  cwd                          ; DX:AX sign-ext g
  pop bx                       ; widened RHS → BX
  idiv bx                      ; AX = quotient, DX = remainder
  mov word ptr DGROUP:_<g>, ax ; (or `, dx` for `%=`)
  ```

  Added a new arm in `emit_compound_assign` gated on
  `int LHS + Type::Char|Type::UChar RHS + Div|Mod`. Signed
  `idiv` is correct for both signed and unsigned byte
  RHS (zero-extended byte is positive).
- `799` — free pass off batch 159's Add/Sub/Bit* arm
  which already accepts `Type::Int` local RHS. Confirms
  the broader arm fires for int locals (closing a
  previously unprobed gap — see fixture 571 only
  covered the int-global RHS case).

## `int` global compound with `char` / `uchar` local RHS

Fixtures `794` (`g += char c`), `795` (`g += uchar c`),
`796` (`g *= char c`) — extending compound coverage from
long-LHS to int-LHS.

- `794` / `795` — Add/Sub/Bit* with byte RHS: the
  existing `emit_expr_to_ax` widening (cbw or `mov ah,
  0`) already produces a 16-bit value in AX, and the
  memory-direct `<op> word ptr DGROUP:_<g>, ax` shape
  is identical to the same op with an int local RHS.

  Added a new int-global-compound arm gated on
  `Type::Int | Type::UInt` LHS, `Add/Sub/Bit*` op, and
  `Type::Int | Type::UInt | Type::Char | Type::UChar`
  local RHS. Placed after the existing global-RHS and
  constant-RHS arms so those continue to take
  precedence. This also unblocks `g += int x` (int
  local RHS) which had been an unprobed gap.

- `796` — int `*= char`: similar register-pressure
  problem as the long `*= uchar` case (fixture 786).
  AX holds the widened RHS after `cbw`, but `imul` on
  16-bit operands consumes AX for the LHS. BCC inserts
  a `push ax; ...; pop dx` shuffle:

  ```
  mov al, byte ptr <c>
  cbw                          ; AX = char as int
  push ax
  mov ax, word ptr DGROUP:_<g> ; LHS
  pop dx                       ; widened RHS → DX
  imul dx                      ; DX:AX = AX * DX
  mov word ptr DGROUP:_<g>, ax ; low-16 stored
  ```

  Added a new arm for int-global `*=` with byte local
  RHS. `imul dx` is signed but produces the correct
  low-16 for any operand combination — BCC also uses
  signed `imul` for `*= uchar` (the zero-extended
  dividend is positive, and the low-16 product matches).

## `ulong` compound `*=` / `/=` with `char` / `uchar` RHS

Fixtures `791` (`g *= char c`), `792` (`g /= char c`),
`793` (`g /= uchar c`) — three free passes confirming
the byte-RHS arms generalize across LHS signedness:

- `791` — `Type::Char + Mul` arm picks `N_LXMUL@`, which
  is sign-agnostic (the helper computes the low-32 of a
  full 64-bit product, identical for both signednesses).
  LHS being unsigned doesn't change the widening shape
  (signed widening of the char via `cbw; cwd`).
- `792` — `Type::Char + Div` arm picks the helper from
  LHS signedness, so `ulong /= char` correctly emits
  `N_LUDIV@`. The widening shape is still signed (`cbw;
  cwd`) since the RHS is a signed char — the C90
  conversion sequence is char → int → long (signed) →
  ulong, and the bit-level result of the signed-to-
  unsigned conversion is identity.
- `793` — `Type::UChar + Div` arm (batch 157's new shape
  with `xor dx, dx; push dx`) also picks helper from LHS
  signedness, so `ulong /= uchar` emits `N_LUDIV@`.

No code changes. The "widening shape from RHS type,
helper from LHS signedness" split holds across all
long-compound arms.

## `long` compound `/=` uchar and `<<=` char / uchar

Fixtures `788` (`g /= uchar c`), `789` (`g <<= char c`),
`790` (`g <<= uchar c`).

- `788` — `/= uchar` is a new shape distinct from `/= uint`
  (fixture 773) for the same register-pressure reason as
  `*= uchar` (fixture 786): the uchar materializes in AX
  (`mov ah, 0`), so BCC can't use AX as the source of the
  pushed `0` for the widened RHS high half. It zeros DX
  instead:

  ```
  mov al, byte ptr <c>
  mov ah, 0                    ; AX = uchar (zero-ext)
  xor dx, dx                   ; DX = 0 (rhs hi)
  push dx
  push ax
  push word ptr <lhs_hi>
  push word ptr <lhs_lo>
  call near ptr <helper>
  ```

  Added a new arm in `emit_compound_assign` gated on
  `long LHS + Type::UChar RHS + BinOp::Div|Mod`. Helper
  picked from LHS signedness (`N_LDIV@`/`N_LMOD@` for
  signed, `N_LUDIV@`/`N_LUMOD@` for unsigned).
- `789` / `790` — free passes after extending the long-
  LHS-shift arm's RHS-type gate from `Type::Int |
  Type::UInt` to `Type::Int | Type::UInt | Type::Char |
  Type::UChar`. The arm reads `CL` directly as `byte ptr
  <addr>`, which works for any RHS width — CL only needs
  the low byte and the C90 shift-count value space
  (0..31 for long) fits in a byte regardless of RHS
  signedness.

## `long` compound `*=` / `/=` with `char` / `uchar` RHS

Fixtures `785` (`g *= char c`), `786` (`g *= uchar c`),
`787` (`g /= char c`).

- `785` — signed `*= char`: same push/pop dance as the
  long `*= int` arm (fixture 762), prefixed by the `cbw`
  step `emit_expr_to_ax` emits for a char-typed local.
  Extended that arm's gate from `Type::Int` to
  `Type::Int | Type::Char`.
- `786` — unsigned `*= uchar`: a new shape distinct from
  the `*= uint` arm (fixture 772) because the uchar lives
  in AX (zero-extended via `mov ah, 0`), which collides
  with the LHS-low load. BCC inserts a `push ax;
  ...; pop bx` shuffle:

  ```
  mov al, byte ptr <c>
  mov ah, 0                    ; AX = uchar (zero-ext)
  xor cx, cx                   ; CX = 0 (rhs hi)
  mov dx, word ptr <lhs_hi>
  push ax                      ; save widened RHS lo
  mov ax, word ptr <lhs_lo>    ; LHS lo
  pop bx                       ; restore as RHS lo (BX)
  call near ptr N_LXMUL@
  ```

  `*= uint` can skip this dance because the uint is loaded
  directly from memory into BX. `*= uchar` cannot —
  the byte→int widening forces AX. Added a new arm in
  `emit_compound_assign` gated on `long LHS + Type::UChar
  RHS + BinOp::Mul`.
- `787` — signed `/= char`: same as `*= char`, just
  extending the existing `/= int` arm's gate to also
  accept `Type::Char`. The push order (high DX, then low
  AX, then LHS halves) is unchanged.

## `long` compound with `int` / `char` / `uchar` RHS

Fixtures `782` (`ulong g += int x`), `783` (`long g += char c`),
`784` (`long g += uchar c`).

- `782` — free pass: the existing `Type::Int` signed-widening
  arm (fixture 755) is not gated on LHS signedness, so
  `unsigned long g += int x` uses the same `cwd` sign-
  extension. The result reinterprets the bit pattern as
  unsigned long, which is correct under C90 conversion
  rules (signed long can represent all signed int values,
  so the int converts to long first; the long-to-ulong
  step is a no-op at the bit level).
- `783` — signed `char` widens to long via **two** stage
  extensions: `cbw` widens AL→AX, `cwd` widens AX→DX:AX.
  `emit_expr_to_ax` already emits the `cbw` step for a
  char-typed local, so extending the signed-widening
  arm's gate from `Type::Int` to `Type::Int | Type::Char`
  lets it pick up char too — the `cwd` already there
  finishes the long-widening:

  ```
  mov al, byte ptr <c>
  cbw                          ; AL → AX (sign-extend)
  cwd                          ; AX → DX:AX (sign-extend)
  add word ptr <lhs_lo>, ax
  adc word ptr <lhs_hi>, dx
  ```
- `784` — unsigned `char` uses the **zero-extension** path
  (no `cwd`): `mov al, <c>; mov ah, 0` zero-extends to int,
  then the same `<hi_op> 0` immediate-form trick from the
  `Type::UInt` arm finishes the long-widening. Extended
  that arm's gate from `Type::UInt` to `Type::UInt |
  Type::UChar`:

  ```
  mov al, byte ptr <c>
  mov ah, 0                    ; AL → AX (zero-extend)
  add word ptr <lhs_lo>, ax
  adc word ptr <lhs_hi>, 0     ; high-half via carry only
  ```

Reuse of `emit_expr_to_ax` for the byte-to-int widening
means no new IR or encoding is needed — the byte-width
step happens before the long compound path even begins.

## `ulong` stack `/= uint`, signed `long` `+= / *= uint`

Fixtures `779` (`a /= x` stack ulong LHS), `780` (`g += x`
signed long LHS), `781` (`g *= x` signed long LHS) — three
more free passes confirming the unsigned-widening arms
don't care about LHS signedness or location:

- `779` — batch 152's `/= uint` arm uses `long_halves_of`
  for the LHS push, which already produces `[bp+off]`
  addresses for a stack-resident long. Helper picked from
  LHS signedness as `N_LUDIV@`.
- `780` — batch 150's `Type::UInt` Add/Sub/Bit* arm is
  not gated on LHS signedness. Signed `long += uint x`
  emits the same zero-extension shape (`add ax; adc 0`).
  The result is a signed long but the bit pattern is
  identical to the unsigned case for these ops.
- `781` — batch 151's `*= uint` arm uses `N_LXMUL@`
  regardless of signedness (the helper is sign-agnostic
  for the low-32 result). LHS signedness is irrelevant
  for the widening; the zero-extension `xor cx, cx` is
  driven only by RHS being `Type::UInt`.

No code changes needed. These complete the
unsigned-widening matrix for compound long operators
against a `uint` RHS variable.

## `ulong` `>>=` uint and stack-LHS `ulong` `+=` / `*=` uint

Fixtures `776` (`g >>= x`), `777` (`a += x` stack LHS),
`778` (`a *= x` stack LHS) — three free passes confirming
the unsigned-widening arms generalize:

- `776` — same shift-by-int arm (fixture 760) that accepts
  both `Type::Int` and `Type::UInt`; LHS signedness picks
  `N_LXURSH@` over `N_LXRSH@`.
- `777` — batch 150's `Type::UInt` Add/Sub/Bit* arm uses
  `long_halves_of`, which already resolves to `[bp+off]`
  addresses for a stack-resident long LHS. The memory-
  direct shape (`add word ptr [bp-N], ax; adc word ptr
  [bp-N+2], 0`) is location-agnostic.
- `778` — batch 151's `*= uint` arm: `bx`/`cx` load and
  call sequence is identical whether the LHS halves live
  in DGROUP or on the stack, since the path materializes
  DX:AX from the LHS regardless.

No code changes needed — these confirm that the unsigned
widening arms didn't accidentally bake in a global-only
assumption.

## `ulong` compound `/=` / `%=` / `<<=` with `uint` RHS

Fixtures `773` (`g /= x`), `774` (`g %= x`), `775` (`g <<= x`).
LHS is `unsigned long` global, RHS is `unsigned int` local.

- `773` — long `/= uint`: zero-extension lets BCC push a
  literal `0` for the widened RHS high half via `xor ax,
  ax; push ax`, then push the uint directly via `push word
  ptr <rhs>` without going through AX (the signed path
  needs AX for the `cwd`). Rest of the call shape matches
  fixture 763's signed `/= int`:

  ```
  xor ax, ax
  push ax                    ; widened RHS high (zero)
  push word ptr <rhs>        ; widened RHS low (uint)
  push word ptr <lhs_hi>
  push word ptr <lhs_lo>
  call near ptr N_LUDIV@
  mov word ptr <lhs_hi>, dx
  mov word ptr <lhs_lo>, ax
  ```

  Added a new arm in `emit_compound_assign` gated on
  `long LHS + Type::UInt RHS + BinOp::Div|Mod`. Helper
  picked from LHS signedness — `N_LUDIV@`/`N_LUMOD@` for
  unsigned LHS, `N_LDIV@`/`N_LMOD@` otherwise.
- `774` — free pass; same arm handles `Mod`.
- `775` — free pass off batch 147's shift-by-int arm,
  which already accepted both `Type::Int` and `Type::UInt`
  for the shift count (only the LHS signedness picks
  `N_LXLSH@` vs `N_LXURSH@`).

## `ulong` compound `|=` / `^=` / `*=` with `uint` RHS

Fixtures `770` (`g |= x`), `771` (`g ^= x`), `772` (`g *= x`).
LHS is `unsigned long` global, RHS is `unsigned int` local.

- `770` / `771` — free passes off batch 150's `Type::UInt` arm
  (bitwise `or`/`xor` against memory with high-half `or 0` /
  `xor 0` is a no-op preserving the zero-extended widening).
- `772` — long `*= uint`: BCC widens the uint by **zero**-
  extension into CX (`xor cx, cx`) — no `cwd`, no push/pop
  dance the signed `*= int` path (fixture 762) needs. Since
  zero-extension doesn't touch DX, BX is free to load from
  the uint directly and the LHS halves slot into DX:AX
  without contention:

  ```
  mov bx, word ptr <rhs>      ; load uint → BX
  xor cx, cx                  ; zero-extend → CX
  mov dx, word ptr <lhs_hi>
  mov ax, word ptr <lhs_lo>
  call near ptr N_LXMUL@
  mov word ptr <lhs_hi>, dx
  mov word ptr <lhs_lo>, ax
  ```

  Added a new arm in `emit_compound_assign` gated on
  `long LHS + Type::UInt RHS + BinOp::Mul`, parallel to the
  signed `*= int` arm at fixture 762. The helper `N_LXMUL@`
  itself is sign-agnostic — only the widening shape changes.

## `long` compound with `unsigned int` RHS (zero-widening)

Fixtures `767` (`g += x` unsigned), `768` (`g -= x`),
`769` (`g &= x`). LHS is `unsigned long`, RHS is `unsigned
int`.

BCC handles unsigned widening with **no widening register**
at all — just an immediate `0` for the high-half operand:

```
mov ax, word ptr [bp-N]    ; load uint RHS
add word ptr DGROUP:_g, ax  ; add to low half
adc word ptr DGROUP:_g+2, 0 ; carry-only propagation into high
```

Same skeleton works for `-=`/`&=`/`|=`/`^=`:
- `+=` / `-=`: high-half op is `adc 0` / `sbb 0` (rides on
  the carry/borrow from the low half).
- `&=`: `and <hi>, 0` zeros the high half — matches the
  zero-extended RHS semantics.
- `|=` / `^=`: `or <hi>, 0` / `xor <hi>, 0` is a no-op,
  preserving the high half.

Added a new arm in `emit_compound_assign` gated on
`Type::UInt` RHS. Reuses the existing `<op>GroupSymImm8Sx`
encoders so the high-half-with-imm-0 step assembles via the
short 5-byte form (`81|83 <modrm> ... 00`).

## `long` compound `%=` int + stack-LHS variants

Fixtures `764` (global `g %= x`), `765` (`a += x` stack LHS),
`766` (`a *= x` stack LHS).

- `764` — free pass off batch 148's `/=`/`%=` arm.
- `765` — needed four new `<op> word ptr [bp+N], <reg16>` IR
  variants for the long-stack += int shape:
  - `AddBpRelAx` (`01 46 dd`) — sibling of the existing
    `AddBpRelDx` (which writes DX for long-long). For the
    int-RHS widening case, AX holds the int low word.
  - `AdcBpRelDx` (`11 56 dd`) — high-half carry partner.
    DX holds the cwd sign-extension.
  - `SubBpRelAx` (`29 46 dd`) and `SbbBpRelDx` (`19 56 dd`)
    — `-=` siblings.
- `766` — free pass; the long-stack-LHS Mul path already
  routed through the same `emit_long_compound_to_mem`-style
  helper with the cwd-widened RHS pushed onto the stack.

The asymmetry between Add/Sub (needing the new
`AddBpRelAx`/`AdcBpRelDx` pair) and Mul (using stack
push/pop) reflects BCC's two strategies: Add/Sub can do the
op directly against memory; Mul has to set up registers in
a specific order before calling the helper.

## `long` compound `>>=` / `*=` / `/=` with `int` RHS

Fixtures `761` (`g >>= x`), `762` (`g *= x`), `763` (`g /= x`).

- `761` — free pass off batch 147's shift-by-int arm.
- `762` — long `*= int`: BCC routes the cwd-widened RHS
  through the stack since `cwd` clobbers DX, which the LHS
  load also needs. Sequence: `mov ax, <x>; cwd; push ax;
  push dx; mov dx, <lhs_hi>; mov ax, <lhs_lo>; pop cx; pop
  bx; call N_LXMUL@; store`. Push/pop ordering places
  RHS-high in CX and RHS-low in BX — matching the helper.
- `763` — long `/= int`: simpler since `N_LDIV@` takes all
  four halves via push, not via registers. BCC pushes the
  widened RHS (high `dx`, then low `ax`), then the LHS's
  two halves, calls the helper. Modulo and unsigned-LHS
  variants take their existing helper-dispatch table.

Asymmetry note: `*=` swaps the push-pop dance to free DX
for the LHS load, while `/=` doesn't need to because the
helper consumes everything off the stack.

## `long` compound `|=` / `^=` / `<<=` with `int` RHS

Fixtures `758` (`g |= x`), `759` (`g ^= x`), `760` (`g <<= x`).

- `758` / `759` — free passes off batch 146's int-RHS arm
  (the bitwise `<op>` is mirrored to both halves with `dx`
  carrying the sign-extension).
- `760` — added a long-LHS-shift-by-int-RHS arm. Same
  helper-call shape as `long <<= long h` (batch 140) but
  the shift count is loaded from a `byte ptr` view of the
  int storage. Note `cl` only needs the low byte regardless
  of whether the RHS is int (16 bits) or long (32 bits), so
  the two shapes converge once `mov cl, byte ptr <addr>`
  fires. Accepts both `Type::Int` and `Type::UInt` for the
  RHS — shift count signedness doesn't affect the result;
  only the LHS signedness picks `N_LXRSH@` vs `N_LXURSH@`.

## `long` compound with `int` RHS (signed widening)

Fixtures `755` (`g += x`), `756` (`g -= x`), `757` (`g &= x`)
— mixed-width compound where the LHS is a long global and
the RHS is an int.

BCC widens the int via `cwd` into DX:AX before applying the
memory-direct compound. For `+=`/`-=`, DX carries the sign-
extension into the high-word add/sub with `adc`/`sbb`. For
bitwise (`&=`, `|=`, `^=`) it applies the **same** op to
both halves with DX — confirming BCC promotes the int to a
signed long even before bitwise ops:

```
mov ax, word ptr [bp-N]    ; load int RHS
cwd                          ; sign-extend AX → DX:AX
add word ptr DGROUP:_g, ax   ; (or sub / and / or / xor)
adc word ptr DGROUP:_g+2, dx ; (or sbb / and / or / xor)
```

Added a new arm in `emit_compound_assign` gated on
`long LHS + Type::Int RHS + Add|Sub|Bit*`, using a new
`rhs_type_for_long_widening` helper for the RHS type
lookup. Added two tasm IR variants:
- `AdcGroupSymDx` (`11 16 lo hi`) — high-half carry partner
  for `long += int`.
- `SbbGroupSymDx` (`19 16 lo hi`) — sibling for `long -= int`.

The bitwise siblings (`and`/`or`/`xor word ptr <g>+2, dx`)
already had their IR variants from batch 139 (the long-long
arm uses AX for the high half; here we use DX, but the
`AndGroupSymReg16`/etc. variants accept any reg).

Unsigned-int RHS (`UInt`) is not yet probed; would use
`xor dx, dx` / `mov dx, 0` instead of `cwd` for the
widening step.

## `long` compound on deref, struct field, and array element

Fixtures `752` (`*p += h` long pointer + long-var RHS),
`753` (`s.x += h` stack struct long field + long-var RHS),
`754` (`a[1] += h` long array + long-var RHS). All three
free passes off pre-existing infrastructure:

- `752` — the long-pointee `*p += y` path (slice 398) was
  already in place; it accepts any non-constant RHS via the
  shared `emit_long_compound_to_mem` helper.
- `753` — the stack-resident struct long-field arm
  (slice 389) routes through the same long-compound-to-mem
  helper with a bp-relative destination.
- `754` — the const-index long array path (slice 393)
  similarly accepts variable RHS through that helper.

The `emit_long_compound_to_mem` helper is unifying enough
that these three target shapes (`[reg]`, `[bp+off]`,
`DGROUP:_<sym>+off`) all reuse the same low/high addr-pair
codepath without per-shape branching.

## `long` mixed-location shift and stack-LHS heavy ops

Fixtures `749` (`g <<= h` global LHS + stack RHS),
`750` (`a *= g` stack LHS + global RHS),
`751` (`a >>= g` stack LHS + global RHS).

- `749` — extended the mixed-location arm to also cover
  `Shl|Shr`. Same `mov cl, byte ptr <rhs_lo>; mov dx,
  <lhs_hi>; mov ax, <lhs_lo>; call N_LXLSH@/...; mov
  <lhs_hi>, dx; mov <lhs_lo>, ax` shape as the both-globals
  path — the `rhs_lo` address string already drops the
  `word ptr` prefix so reusing it as `byte ptr <rhs_lo>`
  Just Works.
- `750` / `751` — free passes off the existing mixed-
  location Mul / Shl|Shr arms. Confirms the
  `long_halves_of` helper symmetrically handles the stack-
  LHS case (helper returns `bp_addr(off)` and
  `bp_addr(off+2)` instead of `DGROUP:_<sym>` / `+2`).

## `long` mixed-location `&=` / `*=` / `/=`

Fixtures `746` (`g &= h` global LHS + stack RHS),
`747` (`g *= h`), `748` (`g /= h`).

- `746` — free pass off batch 142's new bit-arith arm.
- `747` / `748` — needed extending. The new mixed-location
  arm was previously Add/Sub/Bit-only; widened it to cover
  Mul (CX:BX RHS + DX:AX LHS + `N_LXMUL@`) and Div/Mod
  (push both pairs + `N_LDIV@`/`N_LMOD@`/`N_LUDIV@`/
  `N_LUMOD@` by signedness). Both shapes reuse the same
  `long_halves_of` helper to drive the address strings, so
  the body of each arm is identical to the both-globals
  branch with just the format args changed. Shifts not yet
  probed in mixed-location form (helper path would need
  the same generalization).

## `long` compound with mixed global/stack location

Fixtures `743` (`a += b` both stack), `744` (`g += h` global
LHS + stack RHS), `745` (`a += g` stack LHS + global RHS).

- `743` — free pass; pre-existing long-stack-local
  compound path (slices 290/339) handles a stack-local LHS
  with a stack-local RHS uniformly.
- `744` / `745` — needed a new arm. The existing long-
  global-compound branch only matched when *both* operands
  were globals. Added a "long LHS + long RHS regardless of
  location" arm with the same `mov ax,<hi>; mov dx,<lo>;
  <op> <lhs_lo>,dx; <carry> <lhs_hi>,ax` shape, guarded
  with `!(both globals)` so the existing both-globals
  branch keeps firing for fixtures 734-738.
- Introduced small `lhs_long_type` / `rhs_long_type_of_ident`
  / `long_halves_of` helpers to keep the new arm shape-
  uniform regardless of storage location.

## `long` global compound `>>=` / `*=` / `%=` by variable

Fixtures `740` (`g >>= h`), `741` (`g *= h`), `742` (`g %= h`).
All three free passes off pre-existing handlers:

- `740` — batch 140's `Shl|Shr` arm for long-global with
  long-var RHS (signed picks `N_LXRSH@`, unsigned would pick
  `N_LXURSH@`).
- `741` — existing `BinOp::Mul` arm (line 3287) for long-
  global compound: `N_LXMUL@` helper with both operands
  loaded into the convention CX:BX (RHS) / DX:AX (LHS).
- `742` — existing `BinOp::Div | BinOp::Mod` arm: `N_LMOD@`
  helper (signed; unsigned uses `N_LUMOD@`).

The long-global compound-with-long-var arc is now byte-exact
across all five arith ops + the bitwise/shift set.

## `long` global compound `|=` / `^=` / `<<=` by variable

Fixtures `737` (`g |= h`), `738` (`g ^= h`), `739` (`g <<= h`).

- `737` / `738` — free passes off batch 139's
  `BinOp::Add|Sub|BitAnd|BitOr|BitXor` arm for long-global
  with long-variable RHS.
- `739` — long-global shift by long-variable RHS. BCC's
  pattern reuses the K-constant K>1 helper-call shape but
  loads CL from h's low byte: `mov cl, byte ptr DGROUP:_h;
  mov dx, _g+2; mov ax, _g; call N_LXLSH@; mov _g+2, dx;
  mov _g, ax`. Added the branch in the long-global var-RHS
  match alongside the arith/bitwise handler. Helper picks
  `N_LXLSH@` / `N_LXRSH@` / `N_LXURSH@` based on op and
  signedness — same dispatch table as the K-constant path.
- Added `MovReg8GroupSym` tasm IR variant (`8A (mod=00
  reg=<r> r/m=110) lo hi` + FIXUPP) — generic byte-global
  load for non-AL destinations. AL keeps the shorter
  `MovAlGroupSym` (`A0` moffs8 form). Codegen needed this
  for the `mov cl, byte ptr DGROUP:_h` shape.

## `long` global compound with long variable RHS

Fixtures `734` (`g += h`), `735` (`g -= h`), `736` (`g &= h`)
— long-global compound with another long global as RHS.

The existing `long g <op>= b` path (line 3279) only routed
`Mul` and `Div/Mod` (helper calls); `Add/Sub/BitAnd/BitOr/
BitXor` fell through to the local-lookup panic. BCC's
pattern for these:

```
mov ax, word ptr DGROUP:_h+2     ; high of h
mov dx, word ptr DGROUP:_h        ; low of h
<lo_op> word ptr DGROUP:_g, dx    ; e.g. add / sub / and / or / xor
<hi_op> word ptr DGROUP:_g+2, ax  ; matching carry/borrow op for arith
```

For arith, `hi_op` is `adc`/`sbb` (carry/borrow). For
bitwise, `hi_op` is the same as `lo_op` (no carry across
halves). Added the branch and these tasm IR variants:
- `SbbGroupSymAx` — `19 06 lo hi` (high-half borrow partner
  for long-global `-=`, sibling of the existing
  `AdcGroupSymAx`).
- `AndGroupSymReg16` / `OrGroupSymReg16` /
  `XorGroupSymReg16` — `21|09|31 (mod=00 reg=<r> r/m=110)
  lo hi` (long-word siblings of the byte variants from batch
  121).

## `char` update as function argument (stack-resident)

Fixtures `731` (`f(c++)`), `732` (`f(++c)`), `733` (`f(c--)`)
— BCC chose to keep `c` stack-resident here (the function-
arg expression context apparently affects the allocator's
eligibility check; not yet pinned to a rule). The generic
`emit_update_to_ax` previously panicked with "stack-
resident local not yet supported" for any char update.

Added a stack-resident char branch to `emit_update_to_ax`
with the same pre/post asymmetry observed elsewhere:

- **Post** (`c++`): `mov al, byte ptr [bp-N]; inc byte ptr
  [bp-N]; cbw` — captured value first, then memory-direct
  side effect, then widen.
- **Pre** (`++c`): `mov al, byte ptr [bp-N]; inc al; mov
  byte ptr [bp-N], al; cbw` — AL detour. Stack-char pre
  takes the same shape as char-global pre (batch 128) and
  char-field pre (batch 130): BCC threads the new value
  through AL rather than memory-direct `inc byte ptr`.

Unsigned uses `mov ah, 0` for the widening step.

## `char` update as expression result (int destination)

Fixtures `728` (`int r = c++`), `729` (`int r = ++c`),
`730` (`int r = c--`) — char update result widened into an
int destination.

The generic `emit_update_to_ax` path produces the right
*instructions* (load, widen, store, side-effect) but in the
wrong *order* — BCC stores before the side effect on Post,
and threads through AL with explicit write-back on Pre. Added
two more char-aware fast paths in `emit_assign_local`:

- **Char→int Post**: `mov al, <src>; cbw; mov [bp-N], ax;
  inc <src>` — store the widened value, then bump source.
  Unsigned uses `mov ah, 0` for widening.
- **Char→int Pre**: `mov al, <src>; inc al; mov <src>, al;
  cbw; mov [bp-N], ax` — bump AL, write back to source,
  widen, store.

Together with batch 136 (char destination), the byte-source
update-as-expression path now matches BCC for both
destination widths and both pre/post positions.

## `char` update as expression result (char destination)

Fixtures `725` (`d = c++`), `726` (`d = c--`),
`727` (`d = ++c`) — char update result captured into another
char (the batch-135 deferred case).

The generic `emit_update_to_ax` path widens through AL with
`cbw`, which is wasted work when the destination is byte —
BCC keeps everything in AL without the widen. Added two
char-aware fast paths in `emit_assign_local`'s
`LocalLocation::Stack(off)` arm (parallel to the existing
int fast path at line 7253):

- **Post**: `mov al, <src>; mov byte ptr [bp-N], al; inc
  <src>` (store the captured value, then bump source).
- **Pre**: `mov al, <src>; inc al; mov <src>, al; mov byte
  ptr [bp-N], al` (bump AL, write back to BOTH source and
  destination from AL). Note BCC threads the new value
  through AL rather than incrementing the source register
  directly — keeps everything single-source so AL holds the
  expression result for the subsequent store.

The asymmetry vs the int destination path is that BCC also
threads the int through AX with a single store; we don't
yet need a different shape for that since `inc dl` followed
by `mov [bp-N], dl` would skip the AL write. (Confirmed: the
int fast path stores the register directly.)

## `char` parameter compound

Fixtures `722` (`c += 5` on a char param), `723` (`c &= 15`),
`724` (`c += d` between two char params) — all free passes.

- Char parameters are enregistered into the same byte pool
  (DL/BL/CL) as local char variables via the locals planner.
  Once the param is in a byte register, the existing
  CompoundAssign-on-byte-register path (batch 116/117)
  handles all the arith/bitwise/shift ops without
  modification. The probes confirm the param path is
  byte-exact against BCC.

### Deferred — char postinc as expression result

Probed `d = c++;` and observed BCC emits:
```
mov al, dl                 ; load old c
mov byte ptr [bp-1], al    ; store directly as byte (no widen)
inc dl                     ; post-increment c
```
Our codegen instead emits `mov al, dl; cbw; inc dl; mov byte
ptr [bp-1], al`. Two issues:
1. Spurious `cbw` — char-to-char assignment widens through
   AX in our emit_update_to_ax path, but BCC stores AL
   directly when both source and destination are byte.
2. Ordering — BCC stores then increments; we increment then
   store. Same effect but different bytes.

Held until a focused fix lands; replaced this batch's slot
with the char-param `&= 15` free pass.

## `char` stack-local array compound

Fixtures `719` (`a[2] += 5`), `720` (`a[2] &= 15`),
`721` (`a[2]++` postfix discarded), all on `char a[4]` as a
stack local.

- `719` — char-local-array arith. The stack-local arm of
  `emit_array_compound_assign` had only an int-style
  `<op> <width> ptr [bp-N], K` path; for char this is wrong
  (BCC uses the AL detour). Reorganized the arm so that
  char-element arith takes the AL load-modify-store
  (`mov al, byte ptr [bp-N]; add al, K (or inc/dec for
  K=1); mov byte ptr [bp-N], al`) — mirrors the
  char-global-array path from batch 129.
- `720` — char-local-array bitwise stays memory-direct:
  `and byte ptr [bp-N], K`. Added tasm IR variants
  `AndBpRelByteImm8` / `OrBpRelByteImm8` /
  `XorBpRelByteImm8` (encoding `80 66|4E|76 dd ii` — Grp1
  r/m8 imm8 with mod=01 r/m=110).
- `721` — char-local-array postfix `a[K]++` (discarded):
  memory-direct `inc byte ptr [bp-N]`. Same pre-vs-post
  asymmetry as the global path. Added `IncBpRelByte` /
  `DecBpRelByte` tasm IR (`FE 46|4E dd` — Grp4 /0|/1 r/m8
  with mod=01 r/m=110) and parser arms. Codegen branches on
  `from_postfix && store_byte && K=1 && Add|Sub`.

## `char` field / array postfix `++` / `--`

Fixtures `716` (`g.c++`), `717` (`a[2]++`), `718` (`++a[2]`).

- `716` and `717` — same pre-vs-post asymmetry as `g++`
  (batch 128) and `(*p)++` (batch 132), applied to the
  member and array sites. Postfix-discarded compiles to
  memory-direct `inc byte ptr <dest>`; prefix and explicit
  compound use the AL detour. Wired the existing
  `from_postfix` field (added batch 132) through
  `emit_member_compound_assign` and the global-array arm of
  `emit_array_compound_assign`; both gain a "char +
  from_postfix + K=1 + Add|Sub → memory-direct" branch
  before the AL-detour fallthrough.
- `718` (`++a[2]`) — free pass. Confirms BCC takes the AL
  detour for prefix array-element updates, same as
  `++g.c` (fixture 709).

## `char` deref var-RHS and postfix `*p++` / `*p--`

Fixtures `713` (`*p += d`), `714` (`(*p)++`), `715` (`(*p)--`).

- `713` — variable-RHS char-via-pointer arith. BCC loads RHS
  into AL then `add byte ptr [si], al`. Two instructions, no
  AL writeback. Added `AddSiPtrReg8` / `SubSiPtrReg8` tasm
  IR variants (`00|28 04` for AL — `<opcode> (mod=00
  reg=<r> r/m=100)`) and a new arm in
  `emit_deref_compound_assign` for `char-pointee + non-const
  RHS + arith/bitwise`.
- `714` / `715` — postfix `(*p)++` / `(*p)--` (statement
  position, discarded) compiles to memory-direct `inc|dec
  byte ptr [si]` — same pre-vs-post asymmetry as `g++`
  (batch 128). The AST didn't preserve the postfix-vs-
  explicit distinction for `lv++ → lv += 1`; added a
  `from_postfix: bool` field to `MemberCompoundAssign`,
  `DerefCompoundAssign`, and `ArrayCompoundAssign`. Parser
  sets it to `true` only in the postfix-update path. Codegen
  branches on it for the `char + K=1 + arith` case to emit
  memory-direct. Added `IncSiPtrByte` / `DecSiPtrByte` tasm
  IR (`FE 04` / `FE 0C`) and parser arms.
- Probed `++*p` and confirmed BCC uses the AL detour (same
  as `*p += 1`); only the postfix form takes the memory-
  direct path. Member and array siblings of this pattern
  weren't probed yet — `g.c++` is known to behave the same
  way (one probe), but no fixture lands in this batch.

## `char` arrow-field and `*p` compound

Fixtures `710` (`p->c += 5`), `711` (`*p += 5`), `712`
(`*p &= 15`). All three with `p` register-resident in SI.

- `710` and `711` — arith char through a pointer follows the
  same AL detour as char-global: `mov al, byte ptr [si]; add
  al, K; mov byte ptr [si], al`. The writeback step needed a
  new tasm IR variant `MovSiPtrReg8` (`88 (mod=00 reg=<r>
  r/m=100)`, encoding `88 04` for `mov [si], al`) — 8-bit
  sibling of the existing `MovSiPtrReg16`. Codegen:
  - `710` routed through `emit_member_compound_assign`'s
    arrow-with-register-base path; my batch 129 char-field
    arith arm already covered it once the writeback parsed.
  - `711` routed through `emit_deref_compound_assign`'s
    register-pointer fast path (line 5980). Was emitting
    memory-direct `add byte ptr [reg], K`; added the AL
    detour branch with the K=1 inc/dec peephole, mirroring
    the char-field path.
- `712` — char-via-pointer bitwise stays memory-direct:
  `and byte ptr [si], 15`. Added tasm IR variants
  `AndSiPtrByteImm8` / `OrSiPtrByteImm8` /
  `XorSiPtrByteImm8` (encoding `80 24|0C|34 ii` — Grp1 r/m8
  imm8 with mod=00 r/m=100). Codegen already emitted the
  right text via the `mnemonic <width> ptr [reg], K` line;
  only the parser/encoder side needed the new variants.

## `char` struct local, field-var-RHS, and field `++`

Fixtures `707` (`s.c += 5` on stack-resident struct local),
`708` (`g.c += d` with variable RHS), `709` (`++g.c`).

- `707` — free pass. Char struct field on a stack-local
  struct works the same as on a global: the AL load-modify-
  store template substitutes `bp_addr(struct_base +
  field_off)` for `<dest>`. BCC emitted `mov al, byte ptr
  [bp-2]; add al, 5; mov byte ptr [bp-2], al` and our
  codegen produced the same.
- `708` — variable-RHS char-field compound: BCC emits
  `mov al, byte ptr <src>; add byte ptr <dest>, al` —
  memory-direct add against the field, with the RHS pre-
  loaded into AL. Same shape as char-global var (batch
  121). Added an arm to `emit_member_compound_assign` gated
  on `store_byte && op ∈ {Add|Sub|BitAnd|BitOr|BitXor} &&
  try_const_eval(value).is_none()`.
- `709` — `++g.c` parses as `g.c += 1` (the `Update` AST
  node only targets bare identifiers). The AL detour path
  fired but emitted `add al, 1` while BCC emits `inc al`.
  Same byte count, different opcode. Added a K=1 peephole
  in the byte-field arith arm: `add al, 1` → `inc al`,
  `add al, 0xFF` (for `-= 1`) → `dec al`.

## `char` struct field + global-array element compound

Fixtures `704` (`g.c += 5`, struct global, char field at
offset 0), `705` (`g.c &= 15`, char field at offset 2),
`706` (`a[2] += 5`, char global array).

- `704` — char-struct-field arith: BCC uses the same AL
  load-modify-store as for plain char globals
  (`mov al, byte ptr <addr>; add al, K; mov byte ptr
  <addr>, al`). The `<addr>` is `DGROUP:_<name>+<off>` or
  bare `DGROUP:_<name>` when offset is 0. Our codegen was
  using memory-direct `add byte ptr <addr>, K` (which
  tasm's parser doesn't recognize and BCC doesn't emit).
  Extended `emit_member_compound_assign` to branch on
  `store_byte && matches!(op, Add | Sub)` for the AL detour
  with the `add al, (256-K)` canonicalization for `-=`.
- `705` — char-field bitwise stays memory-direct
  (`and byte ptr <addr>, K`) — same asymmetry as
  char-global (batch 122). Free pass off the existing
  fall-through.
- `706` — char-global-array element compound:
  `emit_array_compound_assign` only had a long-global path
  plus a stack-local path; it panicked with "unknown local
  in codegen: a" for non-long globals. Added a global-non-
  long arm that mirrors char-global codegen — same AL
  detour for arith, memory-direct for bitwise, with the
  address being `DGROUP:_<a>+<const_off>` from
  `global_offset_addr`. Int-element globals also route
  through this arm with memory-direct shape.

## `char` global pre vs post: `--g`, `g++`, `g--`

Fixtures `701` (`--g`), `702` (`g++`), `703` (`g--`).

- `701` and `703` — free passes off batch 127's char-global
  update path (pre uses AL detour, post uses memory-direct).
- `702` exposed that **BCC differentiates pre vs post even
  when the result is discarded**. For `++g;` as an
  expression statement, BCC emits the AL load-modify-store
  (`mov al, _g; inc al; mov _g, al`, fixture 700); for
  `g++;` discarded, BCC emits memory-direct
  `inc byte ptr _g` (fixture 702). The two compile to
  *different* machine code despite producing the same side
  effect on `g`. Apparently BCC's pre-update lowering always
  materializes the new value in AL even when caller doesn't
  use it.
- Threaded `UpdatePosition` through `emit_update_in_place`
  and branched the char-global case on it. Pre → AL detour;
  post → memory-direct. Added `IncGroupSymByte` /
  `DecGroupSymByte` tasm IR variants (`FE 06|0E lo hi` +
  FIXUPP, Grp4 r/m8 with mod=00 r/m=110) and the matching
  parser entries (`inc byte ptr ...` / `dec byte ptr ...`).

## `char` global `>>=` / `%=` by variable, plus `++g`

Fixtures `698` (signed `g >>= d`), `699` (signed `g %= d`),
`700` (`++g`).

- `698` — free pass off batch 126's `SarGroupSymByteCl`
  (signed picks SAR for `>>=`).
- `699` — free pass off batch 126's char-global `Div | Mod`
  arm: signed mod stores AH back via `MovGroupSymReg8`
  (added in batch 125).
- `700` — exposed a codegen mismatch for char-global
  `++` / `--`. Our codegen emitted memory-direct
  `inc byte ptr _g` (analogous to the int-global
  `inc word ptr _g` path of fixture 512), but BCC actually
  takes an AL detour for byte globals:
  `mov al, _g; inc al; mov _g, al`. That's consistent with
  the broader BCC pattern — byte arith on globals always
  routes through AL, never memory-direct — even though both
  forms are valid 8086 encodings. Fix: in
  `emit_update_in_place`, branch on `gty.is_char_like()`
  and emit the AL load-modify-store; the existing int-
  global path still emits the memory-direct form.

## `char` global `*=` / `/=` / `<<=` with variable RHS

Fixtures `695` (`g *= d`), `696` (`g /= d`), `697` (`g <<= d`).
Three distinct shapes, all wrapping a memory-resident byte
global, all reusing IR slots from earlier batches:

- `695` — 8-bit `imul byte ptr <src>` through AL:
  `mov al, _g; imul byte ptr [bp-1]; mov _g, al`. No
  widening needed (8-bit imul writes low byte to AL, high
  byte to AH, BCC discards AH). Reuses `ImulByteBpRel`
  (batch 118).
- `696` — signed 8-bit `idiv byte ptr <src>` through AL:
  `mov al, _g; cbw; idiv byte ptr [bp-1]; mov _g, al`. The
  unsigned variant would emit `mov ah, 0; div al, byte ptr
  <src>` (codegen branches but no unsigned-char-global var
  fixture lands yet); both store AL for `/=` and AH for
  `%=`. Reuses `IdivByteBpRel` / `DivByteBpRel`.
- `697` — memory-direct shift by CL, no AL detour:
  `mov cl, byte ptr [bp-1]; shl byte ptr _g, cl`. Added
  three new tasm IR variants —
  `ShlGroupSymByteCl` / `SarGroupSymByteCl` /
  `ShrGroupSymByteCl` — encoded as `D2 /4|/7|/5` with
  ModR/M `mod=00 r/m=110` + disp16 + FIXUPP (e.g. `D2 26
  lo hi` for shl). The shift-by-CL parser arms now try
  `parse_byte_group_symbol` after `Reg8::parse` / before
  `Reg16::parse`.

## `char` global `%=` / `*=` non-p2 / unsigned `/=`

Fixtures `692` (signed `g %= 5`), `693` (signed `g *= 3`),
`694` (unsigned `g /= 4`).

- `692` — same 16-bit `cwd; idiv bx` chain as fixture 691's
  `/=`, but the store target is **DL** (low byte of the DX
  remainder) rather than AL. Required a new tasm IR variant
  `MovGroupSymReg8` (`88 (mod=00 reg=<r> r/m=110) lo hi` +
  FIXUPP) — the generic byte-store-to-global form for non-AL
  sources; AL keeps the shorter `MovGroupSymAl` (`A2`).
  Codegen's existing `BinOp::Div | BinOp::Mod` arm already
  picked `dl` for `Mod`, so widening the arm to accept `Mod`
  alongside `Div` was the only change.
- `693` — non-power-of-2 char-global `*= K`: BCC emits a
  16-bit signed multiply through DX (not BX): `mov al, _g;
  cbw; mov dx, K; imul dx; mov _g, al`. Codegen's `*= K`
  arm now branches inside on `(k & (k-1)) == 0` and emits
  the unrolled `shl` shape only for power-of-2; everything
  else takes the `cbw; mov dx, K; imul dx` path. The
  register asymmetry vs `/=` (BX) is curious — BCC may pick
  DX for `imul` because BX is reserved for indirect-load
  patterns; not yet pinned to a hard rule.
- `694` — unsigned-char global `/= K`: same 16-bit chain as
  the signed case but with `mov ah, 0` instead of `cbw` for
  widening. Surprisingly BCC keeps `cwd; idiv bx` (signed
  divide) even for unsigned — the zero-extended dividend
  fits in `[0, 255]` which is comfortably within the
  positive `idiv` range. Codegen's `/=` arm now branches on
  `gty.is_unsigned()` for the widening step only; the rest
  of the chain is shared.

## `char` global `>>=` / `*=` / `/=` const

Fixtures `689` (`g >>= 2`), `690` (`g *= 4`), `691` (`g /= 4`).

- `689` — free pass off batch 123's shift-byte-one unroll
  (`sar byte ptr _g, 1` × 2, signed char picks SAR).
- `690` — char-global `*= K` for K a power of two:
  load-modify-store through AL with `shl al, 1` unrolled
  log2(K) times. Same shape as the char-local `*= K` path
  (fixture 633). Added a codegen arm gated on the
  power-of-two check; non-power-of-2 multipliers are not yet
  probed and likely use a `mov bl, K; imul bl` chain.
- `691` — char-global `/= K`: load via `mov al, _g`,
  sign-extend with `cbw`, load divisor to BX, `cwd; idiv
  bx`, store quotient back. Mirrors char-local-const
  (fixture 640); the divisor goes through BX regardless of
  K's value (no power-of-2 shortcut, since signed-div
  rounding diverges from arithmetic shift for negatives).
  The arm currently restricts to `Type::Char` (signed); the
  unsigned-char path would use `div` and may have a
  different widening / pool shape — held until probed.

## `char` global `<<=` const and `|=` / `^=` const free passes

Fixtures `686` (`g |= 8`), `687` (`g ^= 31`), `688` (`g <<= 2`).

- `686` / `687` — free passes off batch 122's bitwise mem-
  direct shape: the `OrGroupSymImm8` / `XorGroupSymImm8`
  encoders and parser entries added then already handled
  these. Codegen's bitwise-const arm already covered all
  three of `&|^`.
- `688` — needed a new shape. BCC's `g <<= K` for char
  global unrolls into K memory-direct `shl byte ptr _g, 1`
  (encoding `D0 26 lo hi` + FIXUPP) — the same unroll
  pattern as the int-global path but with the 8-bit `D0 /4`
  opcode instead of the 16-bit `D1 /4`. Added
  `ShlGroupSymByteOne` / `SarGroupSymByteOne` /
  `ShrGroupSymByteOne` tasm IR variants and parser arms
  (each `parse_*_one` now tries `parse_byte_group_symbol`
  before falling through to register). Codegen path picks
  signedness via `gty.is_unsigned()` (signed char → SAR,
  unsigned char → SHR for `>>=`).

## `char` global compound with constant RHS

Fixtures `683` (`g += 5`), `684` (`g -= 7`), `685` (`g &= 15`)
— constant-RHS slice of char-global compound. The crash was
the same as batch 121 (`location_of("g")` panics because g
is global); the codegen shape is different from the
variable-RHS path though, so it gets its own arm.

- **Arith (`+=` / `-=`)**: load-modify-store through AL:
  `mov al, byte ptr _g; add al, K; mov byte ptr _g, al`. BCC
  always emits `add` even for `-=` — the immediate is the
  two's-complement negation (e.g., `g -= 7` →
  `add al, 249`). This matches the broader BCC pattern from
  batch 86-era ("canonicalize `c -= K` as `add <reg>, -K`").
- **Bitwise (`&=` / `|=` / `^=`)**: memory-direct, one
  instruction: `<op> byte ptr _g, K` — encoded as
  `80 (mod=00 reg=/n r/m=110) lo hi ii` + FIXUPP. The
  asymmetry vs int globals (which use memory-direct for
  arith too via `add word ptr _g, K`, fixture 519) is
  empirical — apparently BCC's byte-arith path always takes
  the AL detour.
- Added IR variants:
  - `MovGroupSymAl` — AL→moffs8 store (`A2 lo hi`).
    Companion to the existing `MovAlGroupSym` (load).
  - `AndGroupSymImm8` / `OrGroupSymImm8` /
    `XorGroupSymImm8` — `80 /4` / `/1` / `/6` r/m8 imm8
    against a global. Encoded as `80 26|0E|36 lo hi ii`.
- Codegen: new arm in `emit_compound_assign` keyed on
  `globals.type_of(name) == Char|UChar`, op in the arith-
  bitwise set, and `try_const_eval(value).is_some()`. The
  arith/bitwise split is internal to the arm — both shapes
  share the same gate.

## `char` global compound with variable RHS

Fixtures `680` (`g += d`), `681` (`g -= d`), `682` (`g &= d`)
— first char-global compound-with-variable-RHS fixtures. The
existing global-compound path only knew `int`/`uint` and
`long`-like targets; char targets fell through to the
`location_of(name)` panic ("unknown local in codegen") because
codegen looked up the global name as a local.

- BCC's pattern is two-instruction: load the RHS byte into AL
  (`mov al, byte ptr <src>`), then memory-direct
  `<op> byte ptr DGROUP:_<g>, al`. The accumulator register is
  always AL (BCC never uses other byte regs here, even when
  the RHS is itself in a byte register — it still routes
  through AL).
- Added `AddGroupSymReg8` / `SubGroupSymReg8` /
  `AndGroupSymReg8` / `OrGroupSymReg8` / `XorGroupSymReg8`
  tasm IR variants — byte siblings of the existing
  `AddGroupSymReg16` / `SubGroupSymReg16`. Encoding shape is
  uniform: `<opcode> (mod=00 reg=<r8> r/m=110) lo hi` +
  FIXUPP for the disp16. Opcodes: `00` / `28` / `20` / `08` /
  `30`. Parser entries gated on `parse_byte_group_symbol(lhs)`.
- Codegen: new arm in `emit_compound_assign` keyed on
  `globals.type_of(name)` being `Char | UChar`, op in the
  arithmetic-bitwise set, and `try_const_eval(value).is_none()`
  (constant RHS path is a separate shape — not yet probed).

## `unsigned char` `/=` / `%=` by variable — `div`-form pool

Fixtures `677` (unsigned `c /= d`), `678` (unsigned `c %= d`),
`679` (unsigned `c *= d`). The first two closed the
batch-119-deferred allocator drift for `div`-form byte
operations; the third was a free pass.

- BCC's TASM listing for unsigned byte division includes an
  explicit accumulator operand — `div al,byte ptr [bp+N]`
  rather than the bare-form `idiv byte ptr [bp+N]` used for
  signed. The bytes encode the standard `F6 /6` with ModR/M
  `76 dd`; the `al,` is just textual. Added `DivByteBpRel`
  tasm IR variant with a dedicated parser arm (`"div"` =>
  strips a leading `al,` from the operand) so the listing
  matches.
- Codegen: extended the byte-target `Div | Mod` arm of
  `emit_compound_assign_reg` to branch on
  `locals.type_of(name).is_unsigned()` and emit the
  `mov ah, 0; div al,<src>` shape for unsigned (vs `cbw;
  idiv <src>` for signed). The result store for `%=` reads
  from AH in both shapes.
- **Allocator** — BCC's pool changes for the
  `div`-with-`mov-ah-0` shape: DL is dropped (reason still
  unverified — see the batch-119 deferred note; this batch
  pinned only the empirical *order*, not the *why*) and the
  remaining slots are `[BL, CL]` (natural order — not the
  reversed `[CL, BL]` used by the signed-16-bit-form pool,
  where BL is consumed by the divisor). Added
  `Reg::CHAR_POOL_UDIV = [BL, CL]` and a new
  `body_has_uchar_byte_div_or_mod` walker that fires on any
  unsigned-char compound `/=`/`%=` with non-constant RHS;
  pool selection prioritizes the UDIV variant over
  `CHAR_POOL_DIV` when both could match (UDIV is the more
  specific shape since the signed 16-bit form needs `BX`
  anyway).
- `679` (`c *= d`, unsigned) was a free pass: BCC uses `imul`
  (signed instruction) even for unsigned char multiply
  because the low-byte result is identical, and DL stays in
  the pool (the multiply doesn't trigger the
  div-with-`mov-ah-0` rule).

## `char` `%=` by variable, plus `unsigned char` enregistration

Fixtures `674` (signed `c %= d`), `675` (`unsigned char c >>= d`),
`676` (`unsigned char c += d`).

- `674` — signed `c %= d` was a free pass off batch-118's
  `BinOp::Div | BinOp::Mod` byte arm. BCC keeps c in DL and
  stores the remainder via `mov dl, ah` (8-bit `idiv`'s
  remainder lives in AH).
- `675` / `676` — `unsigned char` enregistration was broken:
  `crates/bcc/src/codegen/locals.rs` filtered char-pool
  eligibility on `Type::Char` only, leaving every `unsigned
  char` local stack-resident and tripping the
  "compound assignment on stack-resident" panic in codegen.
  Widened both filters in the planner to `Type::Char |
  Type::UChar`. The signedness propagates correctly downstream
  via `is_unsigned()` (used in the shift-mnemonic pick and in
  return-widen `cbw` vs `mov ah, 0`).

### Deferred — unsigned char `/=` / `%=` register-allocation drift

While probing, BCC's allocator visibly diverges from our pool:
- Signed `c /= d` / `c %= d` (8-bit form) → c in **DL**.
- Unsigned `c /= d` / `c %= d` (8-bit form, uses `div` and
  `mov ah, 0`) → c in **BL**, not DL.

This is independent of the existing `cwd`-clobber heuristic
(neither shape emits `cwd`). Hypotheses: a separate "AH-as-
widen-temp" gate, or BCC has a distinct pool order for
unsigned byte div/mod. The TASM listing also uses a different
syntax for unsigned (`div al, byte ptr [bp-1]`) vs signed
(`idiv byte ptr [bp-1]`) — the explicit AL hints at a separate
encoder path on BCC's side. Held until a probe pins it down;
fixture slot used for an `unsigned char c += d` free-pass
instead.

## `char` compound `<<=` / `*=` / `/=` by variable

Fixtures `671` (`c <<= d`), `672` (`c *= d`), `673` (`c /= d`)
— closes out the char-compound-by-variable arc.

- `<<=`: free pass — batch-117's `Shl|Shr` byte arm in
  `emit_compound_assign_reg` already covered `Shl`.
- `*=`: BCC uses the 8-bit single-operand `imul byte ptr <src>`
  (`F6 /5`). Added `ImulByteBpRel` tasm IR variant. Codegen
  emits `mov al, <reg>; imul byte ptr <src>; mov <reg>, al`.
- `/=` / `%=`: BCC uses the 8-bit `idiv byte ptr <src>`
  (`F6 /7`) — *not* the 16-bit `cwd; idiv bx` shape used for
  const-RHS char div. Added `IdivByteBpRel` tasm IR. Codegen
  emits `mov al, <reg>; cbw; idiv byte ptr <src>; mov <reg>,
  al|ah`. The 8-bit form has no `cwd`, so DX is preserved.
- **Allocator refinement** (`crates/bcc/src/codegen/locals.rs`):
  `body_has_div_or_mod` previously triggered `CHAR_POOL_DIV`
  ([CL, BL], dropping DL) for any compound `/=` / `%=`. That
  was overly aggressive: only the 16-bit form emits `cwd`,
  and char compound with non-constant RHS uses the 8-bit form.
  Threaded a `char_locals: &HashSet<&str>` through the walker
  and skip the `cwd`-emitting count when the target is in
  that set *and* the value is non-constant
  (`try_const_eval(value).is_none()`). With the refinement,
  fixture 673's `c` stays in DL (matching BCC) instead of
  being demoted to CL.

## `char` compound `|=` / `^=` / `>>=` by variable

Fixtures `668` (`c |= d`), `669` (`c ^= d`), `670` (`c >>= d`)
— the second slice of char-compound-by-variable.

- `|=` / `^=`: added `OrReg8Reg8` (`0A`) and `XorReg8Reg8`
  (`32`) tasm IR variants, mirroring batch-116's
  `AddReg8Reg8`/`SubReg8Reg8`/`AndReg8Reg8`. Codegen branch in
  `emit_compound_assign_reg` was widened to accept `BitOr` and
  `BitXor` alongside the batch-116 set; same `mov al, byte
  ptr <src>; <op> <reg>, al` pattern.
- `>>=`: BCC's variable-count byte shift is `mov cl, byte ptr
  <src>; sar <reg>, cl` (signed `char` picks SAR), encoded as
  `D2 (mod=11 /4|/5|/7 r/m=<reg>)`. Added `ShlReg8Cl` /
  `SarReg8Cl` / `ShrReg8Cl` tasm IR variants — siblings of
  `ShlReg16Cl`/`SarReg16Cl`/`ShrReg16Cl` from batch 56-era.
  Parser shares the same `<op> <reg>,cl` slot and tries
  `Reg8` before `Reg16` (no name overlap).
- Added a `reg.is_byte() && matches!(op, Shl | Shr)` arm to
  `emit_compound_assign_reg`, placed before the
  `BitAnd|BitOr|BitXor|Add|Sub` arm. The signedness comes from
  `locals.type_of(name).is_unsigned()` — same convention as
  the constant-RHS path.

## `char` compound `+=` / `-=` / `&=` by variable

Fixtures `665` (`c += d`), `666` (`c -= d`), `667` (`c &= d`)
— first char-compound-by-variable fixtures, all with c in DL
and d at `[bp-1]`. BCC's pattern is to load the RHS byte into
AL with `mov al, byte ptr <src>` and then apply the op
register-to-register on the byte destination: `add dl, al`
(`02 D0`), `sub dl, al` (`2A D0`), `and dl, al` (`22 D0`).

- Added `AddReg8Reg8` / `SubReg8Reg8` / `AndReg8Reg8` tasm IR
  variants. Encoding is `<op-opcode> (mod=11 reg=<dst>
  r/m=<src>)`, opcodes `02` / `2A` / `22`. These are the first
  `r/m8, r/m8`-pair instructions in the tasm IR — previously
  byte arithmetic only existed against immediates
  (`AddAlImm8`, `AndReg8Imm8`, etc.).
- Added the variable-RHS arm to `emit_compound_assign_reg` in
  `crates/bcc/src/codegen/mod.rs`, gated on
  `reg.is_byte() && matches!(op, Add | Sub | BitAnd)`. The
  branch sits between the existing `Mul`/`Div`/`Mod`/`Shl`/
  `Shr` shortcuts (which require a constant RHS) and the
  `!reg.is_byte()` assert that previously fired for variable
  RHS. The branch uses `resolve_operand_source` and its
  `.byte()` formatter — note that `byte()` still panics for a
  byte-register-resident RHS, which is fine until a fixture
  shows BCC choosing that allocation.

## `*=` / `/=` / `%=` by variable — free pass

Fixtures `662` (`x *= y`), `663` (`x /= y`), `664` (`x %= y`),
all with x in SI and y at `[bp-2]`, all matched without any
new code. The batch-111 `imul <mem>` and batch-112 `idiv <mem>`
work that introduced the direct-memory forms for the constant-
RHS path also handles the variable-RHS path because the
codegen condition was already `matches!(src, Local | Global |
GlobalOffset)` rather than a tighter constant check. No
parser, encoder, or codegen change was required.

### Deferred from batch 88

- Probed `int a[5]; return sizeof(a);` (`582` first draft).
  Diff showed our prologue/epilogue still allocates the frame
  (`sub sp, 10` + `mov sp, bp`) while BCC elides both because
  the array is never referenced at runtime — only in `sizeof`,
  which is constant-folded at parse time. The fix is a frame-
  elision pass: skip the slot for any local whose only uses are
  inside `sizeof`. Probe replaced with int-global postdec until
  we have appetite to thread "live local" tracking into the
  locals planner.
- Probed `int a[5]; int i = 2; return a[i + 1];` (`583` first
  draft). Our codegen panics at `emit_array_addr_to_bx` with
  "non-ident array index not yet supported (no fixture)" — only
  bare-ident array indices route through that path; a `BinaryOp`
  index needs an `emit_expr_to_ax`/`mov bx, ax` prefix instead.
  Probe replaced with the logical-not-of-compare variant until
  the non-ident array index path lands.

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
