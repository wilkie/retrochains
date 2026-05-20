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

## Narrowing-cast complete: OR/XOR/SHL also byte-width

Fixtures `1541` (`(char)(a | b)`), `1542` (`(char)(a
^ b)`), and `1543` (`(char)(a << 2)`) all pass on the
first capture and complete the narrowing-cast
propagation calibration.

- `1541`: `or al, [bp-4]` — byte OR (opcode `0x0A`).
  ✓ OR propagates.
- `1542`: `xor al, [bp-4]` — byte XOR (opcode `0x32`).
  ✓ XOR propagates.
- `1543`: `shl al, 1 / shl al, 1` — byte form `shl
  r/m8, 1` (opcode `0xD0 /4`). The K ≤ 3 unroll
  threshold also applies in byte-width, just on a
  byte register instead of AX. ✓ SHL propagates (for
  small K).

Final propagation table for `(char) (a op b)`:
| Op  | Byte propagated? | Byte-form opcode | Notes |
|-----|------------------|------------------|-------|
| ADD | yes              | `0x02`           | |
| SUB | yes              | `0x2A`           | |
| AND | yes              | `0x22`           | |
| OR  | yes              | `0x0A`           | |
| XOR | yes              | `0x32`           | |
| SHL | yes              | `0xD0 /4` (K≤3) / `0xD2 /4` (K≥4) | K<8 safe |
| MUL | **no**           | n/a              | stays `F7 /5` word |
| DIV/MOD | not probed   | —                | likely no (high-byte dep) |
| SHR | not probed       | —                | safe if K<8 |

So BCC's narrow-cast pass has an allow-list of:
add, sub, and, or, xor, shl. Multiplication is
deliberately excluded — even though
`(char)(a*b) == (char)((char)a*(char)b)`
mathematically and 8086 has `mul r/m8`, BCC keeps it
word-wide.

For the encoder: when codegen encounters `(char) (a
op b)` for any op in the allow-list, switch the
binop emission from word form to byte form (using AL
as accumulator, byte-form ModR/M, and `cbw` for
extension on use), and remove the explicit `and ax,
0xff` / sign-truncate step.

## Narrowing-cast propagation calibrated: ADD/SUB/AND yes, MUL no

Fixtures `1538` (`(char)(a - b)`), `1539` (`(char)(a
& b)`), and `1540` (`(char)(a * b)`) all pass on the
first capture. They further calibrate the byte-width
propagation under `(char)` cast first seen in
[[batch-406-cast-strpool]] / fixture `1535`.

- `1538`: `(char)(a - b)` lowers to **byte-width
  SUB** — `mov al,[bp-2] / sub al,[bp-4] / cbw`
  using opcode `0x2A` (`sub r8, r/m8`). ✓ Sub joins
  the byte-propagation family.
- `1539`: `(char)(a & b)` lowers to **byte-width
  AND** — `mov al,[bp-2] / and al,[bp-4] / cbw`
  using opcode `0x22` (`and r8, r/m8`). ✓ AND joins
  the family too.
- `1540`: `(char)(a * b)` does **NOT** propagate.
  Code: `mov ax,[bp-2] / imul word [bp-4] / cbw`
  — full word-width `imul r/m16` (opcode `0xF7 /5`)
  even though `(char)(a*b) == (char)((char)a*(char)b)`
  mathematically and `imul r/m8` (single-byte form
  with AL implicit) exists on the 8086. BCC's
  byte-propagation pass deliberately excludes MUL.

Updated propagation table for `(char) (a op b)`:
| Op  | Byte propagated? | Byte-form opcode |
|-----|------------------|------------------|
| ADD | yes              | `0x02`           |
| SUB | yes              | `0x2A`           |
| AND | yes              | `0x22`           |
| OR  | (not yet probed) | `0x0A`           |
| XOR | (not yet probed) | `0x32`           |
| MUL | **no**           | n/a (stays `F7 /5` word) |

So far: arithmetic mod-2^k closed ops + bitwise AND
propagate; MUL deliberately stays word-wide. Likely
the IR's narrow-cast pass has a fixed allow-list of
binops keyed by `byte-form encoding is available *and*
preserves low-byte equality with word-form`.

## `(char)(a+b)` byte-width add, no string-literal pooling

Fixtures `1535` (`return (char)(a + b);` — narrowing
cast over int addition), `1536` (`f("Hi") + f("Hi")`
— same string literal in two distinct positions), and
`1537` (`f("Hi") + f("Bye")` — different literals)
all pass on the first capture.

- `1535` (**major finding**): BCC propagates the
  narrowing `(char)` cast *backwards* into the
  binop. Rather than `mov ax,[a] / add ax,[b] / cbw`
  (4 byte plus extension), it emits `mov al,[bp-2]
  / add al,[bp-4] / cbw` — a **byte-width add**
  (opcode `0x02`, `add r8, r/m8`) operating on just
  the low bytes of `a` and `b`. This is
  semantically equivalent (addition mod 2^8 ≡
  truncation of mod 2^16) but generates different
  bytes. The IR has a "narrow-cast-aware" pass that
  rewrites `(char)(x op y)` to byte-width op + sign-
  extend for ops where the low byte of the int
  result equals the low byte of the byte-width
  result (true for add/sub/and/or/xor/shl with
  small counts; NOT for div/mod which depend on
  high bytes). Must replicate byte-exactly.
- `1536` and `1537` together prove **BCC does *not*
  pool string literals**, even identical ones.
  Fixture `1536` has the data segment contain `48
  69 00 48 69 00` — *two* copies of `"Hi\0"`. The
  second call's `mov ax, 3` selects offset 3 (the
  second copy). If BCC pooled, both would resolve
  to offset 0 and the data would be just `"Hi\0"`.
  Fixture `1537` is structurally identical but with
  different content (`"Hi\0Bye\0"`). Each literal
  occurrence in source produces one fresh copy in
  the OBJ's `_DATA` segment.

Implication for the encoder:
- narrowing-cast propagation is opcode-sensitive —
  add/sub/and/or/xor can lower to byte-width
  variants under `(char)` / `(unsigned char)`
  casts; div/mod and shifts beyond 7 must stay
  word-width.
- the string-literal emission must keep each source
  occurrence as a distinct LEDATA segment entry,
  even when contents match.

## addr-taken → forced to memory, `(uchar)int` vs `(char)int` cast widening

Fixtures `1532` (ternary picks between `&x` and
`&y`, then stores through the resulting pointer),
`1533` (`int x; return (unsigned char)x;` zero-
extend cast on int), and `1534` (`int x;
return (char)x;` signed cast on int) all pass on the
first capture.

- `1532` (**refinement**): `x` and `y` both have
  their addresses taken (`&x`, `&y`). They stay on
  the stack (`[bp-2]`, `[bp-4]`) regardless of their
  use count — the address would have nowhere to
  point if they lived in a register. So
  **address-taken locals are forced to memory** as
  an *additional* constraint on top of the
  use-count rule. The ternary itself lowers
  straightforwardly: cmp / inverse-jcc / `lea ax,
  [&x]` arm / jmp / `lea ax, [&y]` arm. Pointer `p`
  goes to SI; `*p = 99` is `mov [si], 99`.
- `1533`: `(unsigned char)x` on an int lowers to
  **`mov al, [bp-2] / mov ah, 0`** — byte-load then
  zero-extend. Same widening idiom as the char →
  unsigned char cast in [[batch-402-comma-cast-shr]]
  fixture `1524`. The cast is implemented as
  "ignore high byte of source, then zero-extend in
  destination" without an explicit AND.
- `1534`: `(char)x` on an int lowers to **`mov al,
  [bp-2] / cbw`** — byte-load then signed sign-
  extend. The 1-byte `cbw` (opcode `0x98`) saves a
  byte versus the `mov ah, 0` (`b4 00`, 2 bytes) of
  the unsigned variant. So the encoder produces:
  | Cast | Sequence | Bytes |
  |------|----------|-------|
  | `(unsigned char)int` | `mov al,[m] / mov ah,0` | 5 |
  | `(char)int`          | `mov al,[m] / cbw`      | 4 |

So the encoder treats narrowing-then-implicitly-
widening as a byte-load of the low part followed by
the appropriate sign/zero extension. Signed casts
get the shorter encoding for free via `cbw`.

## Globals never enregister, `int *p` enregisters, reversed-cmp normalised

Fixtures `1529` (global `int g` written and read
multiple times), `1530` (pointer parameter `int *p`
dereferenced twice), and `1531` (`for (i=0; 3>i;
i++)` — reversed cmp operand order) all pass on the
first capture.

- `1529` (**important rule**): **globals never get
  enregistered**, regardless of use count. Each
  `g = g + 1` lowers to `a1 [_g] / inc ax / a3
  [_g]` (`mov ax,[_g] / inc ax / mov [_g], ax`).
  The final `return g` re-loads from memory again
  with another `mov ax, [_g]`. So the use-count
  heuristic is **scoped to locals and parameters
  only** — globals always stay in static storage,
  presumably because they may be aliased through
  pointers or modified by other translation units /
  interrupts. The `a1`/`a3` opcodes are the AX-with-
  direct-addr short forms (3 bytes each).
- `1530`: pointer parameter `int *p` with two
  derefs (`*p + *p`) enregisters into SI on entry —
  `mov si, [bp+4] / mov ax, [si] / add ax, [si]`.
  Both `[si]` reads share the same register
  (declaration order #1 → SI). Confirms the use-
  count rule applies to *all* parameter types, not
  just plain ints.
- `1531`: source `3 > i` is normalised to **`i < 3`**
  before codegen. The for-loop test emits `cmp si,
  3 / jl body` — with the variable on the **left**
  side of the cmp regardless of which side it
  appeared on in the source. So BCC has an IR-level
  peephole that puts the variable on the cmp's LHS
  (commuting if needed) and adjusts the jcc to
  preserve semantics. Without it, `cmp 3, si` would
  need different jcc selection.

Implication for the Rust reimplementation:
- the IR layer must normalise `K op var` to `var
  inv-op K` for the relops before emitting cmps;
- the codegen pass must distinguish "is this a
  global?" early and never consider globals for
  register allocation.

## Function params enregister like locals (use-count ≥ 2 → SI/DI/...)

Fixtures `1526` (param `x` used 3x: `x*x + x`),
`1527` (param `x` used 2x: `x+x`), and `1528` (two
params `a` and `b` each used 2x in `(a-b)*(a+b)`)
all pass on the first capture and extend the
enregistration model to function parameters.

- `1526`: `_f(int x)` reads from `[bp+4]` (the cdecl
  first-arg slot) **once** into SI on entry — `mov
  si, [bp+4]`. All three uses of `x` (`x*x` first
  factor, `x*x` second factor, the trailing `+x`)
  then operate on SI. So a multi-use param is
  promoted into a register, the same as a multi-use
  local. The arg slot at `[bp+4]` is never reloaded.
- `1527`: `_f(int x)` with `x+x` (2 uses) similarly
  enregisters `x` → SI via `mov si, [bp+4] / mov
  ax,si / add ax,si`. Confirms the threshold is the
  same ≥ 2 syntactic uses, including for parameters.
- `1528`: two parameters, each used twice. **`a` →
  SI** (`mov si, [bp+4]`), **`b` → DI** (`mov di,
  [bp+6]`). Declaration order matches the
  register-allocation order. The intermediate `(a+b)`
  is computed into DX (a scratch register) before the
  `imul`. Confirms params occupy `[bp+4]`, `[bp+6]`,
  ... in cdecl, and that BCC's allocator treats them
  uniformly with locals — the use-count heuristic
  doesn't distinguish param-from-local.

Implication for the encoder: when a function body
has multi-use parameters, BCC always emits the
`mov REG, [bp+N]` copy on entry (after the prologue
push of REG), and then never touches the stack slot
again. The Rust reimplementation needs to walk the
function body to classify each parameter's syntactic
use count *before* emitting the prologue.

## for-comma init, `(unsigned char)c` zero-extend via `mov ah,0`, sar by 15

Fixtures `1523` (`for (i=0, s=0; i<3; i++) s += i;`),
`1524` (`char c = -1; int u = (unsigned char)c;`),
and `1525` (`int v=-1; return v >> 15;`) all pass on
the first capture.

- `1523`: the comma operator inside a for-init is
  flattened — `i = 0, s = 0` produces *identical*
  code to two separate statements: `xor si, si` then
  `xor di, di`. Both locals enregister into SI/DI
  (multi-use across loop body + cmp). The rest of the
  for-loop shape matches [[batch-383-and-not-for-
  fill]]'s template. So `(stmt1, stmt2)` in init is
  pure parser sugar — no special codegen.
- `1524` (**finding**): `(unsigned char)c` lowers to
  `mov al, [bp-1] / mov ah, 0` — the **zero-extend
  widening pattern** (`b4 00`, 2 bytes). This is
  distinct from the signed-char promotion `cbw` (1
  byte) seen in many other fixtures. Note BCC chose
  `mov ah, 0` over the equally-sized `xor ah, ah`
  (`30 e4`, also 2 bytes) — apparent preference for
  the `mov-imm` form. After widening, the int store
  goes through the 4-byte stack slot for `u`.
- `1525`: confirms the shift threshold is purely
  encoding-driven, not value-driven — `v >> 15`
  still uses `mov cl, 15 / sar ax, cl` (3 bytes
  total). There is no unroll up to bit-width even
  when the shift count is large and would seem
  candidate for special handling. The K ≥ 4
  cl-loaded variant remains regardless of how close
  to the int width K gets.

## `v*100` via `imul r/m`, `cmp [bp-2],100` imm8-sext, `100 - v`

Fixtures `1520` (`int v=5; v *= 100;`), `1521` (`if
(v < 100)` against stack-resident v), and `1522`
(`v = 100 - v` — non-commutative subtract with imm
on left) all pass on the first capture.

- `1520`: `v *= 100` lowers to `mov dx, 100 / mov
  ax, si / imul dx / mov si, ax`. BCC uses the
  single-operand `imul r/m16` (opcode `0xF7 /5`),
  the only form available on 8086 — DX:AX gets the
  full 32-bit product, low half remains in AX. **DX
  is clobbered** by the multiply, so no other local
  can be live in DX across an `imul`. This explains
  why DX is the *third* enregistration slot
  (clobbered both by call returns and by mul/div
  ops).
- `1521`: confirms **CMP joins ADD and SUB in the
  imm8-sext family**. `cmp [bp-2], 100` lowers to
  `83 7e fe 64` — opcode `0x83 /7`, mod=01 rm=110
  ([bp+disp8]), imm8-sext = 100. So `cmp` uses the
  short encoding for any imm in [-128,127]. Updated
  encoding-policy classification: **arithmetic-with-
  flags** ops (ADD `/0`, SUB `/5`, CMP `/7`) all use
  `83 /N` imm8-sext when available; **bitwise**
  (OR `/1`, AND `/4`, XOR `/6`) always use `81 /N`
  imm16. ADC `/2` and SBB `/3` not yet probed.
- `1522`: `v = 100 - v` lowers to `mov ax, 100 /
  sub ax, si / mov si, ax`. BCC uses the `sub
  r16, r/m16` form (opcode `0x2B`) with AX as
  destination and SI as source — no `neg + add`
  tricks. The constant goes in AX (left operand of
  subtract) and the variable in SI (right operand).

Note for the encoder: when emitting CMP against a
memory operand `[bp+disp]` with imm fitting in
[-128,127], use `83 /7 disp imm8` (4 bytes with
disp8) to match BCC byte-exact, not the `81 /7
disp imm16` (5 bytes) alternative.

## imm8-sext encoding policy: ADD/SUB yes, OR/AND/XOR no

Fixtures `1517` (`x &= 0x7f` with x in SI), `1518`
(`x ^= 0x7f`), and `1519` (`v -= 5`) all pass on the
first capture. Together with the previous batch's
`1515` (`x |= 0xf`) and earlier [[batch-390-rmw-non-
ax]] (`v += K`), they fully characterise BCC's
imm8-sign-extended encoding policy for non-AX
register destinations:

| Op  | Opcode `/N` | imm8-sext form used? | Observation |
|-----|-------------|----------------------|-------------|
| ADD | `83 /0`     | **yes**              | `1487`,`1488` |
| SUB | `83 /5`     | **yes** (`83 ee 05`) | `1519`      |
| OR  | `81 /1` only| no (`81 ce 0f 00`)   | `1515`      |
| AND | `81 /4` only| no (`81 e6 7f 00`)   | `1517`      |
| XOR | `81 /6` only| no (`81 f6 7f 00`)   | `1518`      |

So **arithmetic** ops (ADD, SUB) honour the imm8-
sign-extended short encoding when the immediate fits
in -128..127, saving 1 byte per instruction.
**Bitwise logical** ops (OR, AND, XOR) always use
the imm16 form, even when imm8-sext would be valid
and shorter. The 8086 ISA defines `83 /1` (OR-imm8-
sext), `83 /4` (AND-imm8-sext), `83 /6` (XOR-imm8-
sext) as legal encodings, so this is BCC's selective
choice — likely the encoder's instruction table
simply omits those entries for the logical group.

Practical consequence for the Rust reimplementation:
when emitting AND/OR/XOR with imm in
[-128,127] against a register, **must** use `81 /N
imm16` (4 bytes) to match BCC byte-exact, not the
shorter `83 /N imm8-sext` (3 bytes).

## `v*1024` → `shl cl=10`, `or si, 0xf` imm16 (not imm8), `{0}` still N_SCOPY@

Fixtures `1514` (`int v=4; return v * 1024;` — mul
by large pow2), `1515` (`int x=0x100; x |= 0xf;
return x >> 4;` — OR with small imm then signed
shr), and `1516` (`int a[3] = {0}; a[1] = 42; return
a[1];` — stack array with all-zero brace init) all
pass on the first capture.

- `1514`: confirms the mul-by-pow2 → shift
  optimisation applies for arbitrarily large powers
  of two: `v * 1024` lowers to `mov cl, 10 / shl ax,
  cl`. The shift amount 10 exceeds the unroll
  threshold (K ≥ 4 → cl-loaded variant), as
  expected. So the lowering is: pow2 N → shift by
  log2(N); below 4 → unrolled `shl ax, 1`; at/above
  4 → `mov cl, N / shl ax, cl`.
- `1515`: **inconsistency finding** — for OR with a
  small imm that fits in -128..127, BCC chooses the
  imm16 form `81 /1` (4 bytes total `81 ce 0f 00`)
  rather than the imm8-sign-ext form `83 /1` (3
  bytes `83 ce 0f`), even though the latter is
  legal and shorter. The add/sub family DOES use
  `83 /0` for the imm8 form ([[batch-390-rmw-non-
  ax]]), so the imm8-sign-ext optimisation is
  selective per opcode group. Possibly BCC's
  encoder simply omits the imm8 variant for OR / XOR
  / AND.
- `1516`: all-zero stack-array brace init **still
  uses `N_SCOPY@`** with an all-zero 6-byte
  template in `_DATA`. BCC does *not* take any
  shortcut for the trivially-zero case — no `xor
  ax,ax / mov [bp-N], ax / ...` chain, no `rep
  stosw`. The memcpy-from-template path is the only
  brace-init lowering for stack arrays, regardless
  of the data being uniform zero.

## `++n` on SI-resident local, 3D `a[1][0][1]` folded, `if (bool_var)`

Fixtures `1511` (`int n=5; return f(++n);` — int
pre-increment as call arg), `1512` (3D global int
array with all-constant indexing), and `1513` (bool
materialized into int then used as `if` condition)
all pass on the first capture.

- `1511`: with `n` enregistered into SI (use count 2:
  `++n` + the implicit read for the call arg),
  pre-increment lowers to **`inc si`** (opcode `0x46`,
  1 byte) directly on the register, then `mov ax,si /
  push ax / call _f / pop cx`. The arg-materialisation
  step doesn't reload from memory — the post-`inc`
  register value is used directly. Returns 6.
- `1512`: with all three indices constant, BCC folds
  the multi-dim offset at compile time: `a[1][0][1]`
  = `(1*4 + 0*2 + 1)*sizeof(int) = 10`. The store
  becomes `mov word [_a+0x000a], 7` (a single
  instruction with one LEDATA FIXUPP) and the load
  is `mov ax, [_a+0x000a]`. No `imul` or `shl` for
  any dim — fully folded.
- `1513`: `int x = (a < b); if (x) ...` does **not**
  fuse the bool materialisation with the test. BCC
  emits the full template (`cmp / jge / mov ax,1 /
  jmp / xor ax,ax`) into x's stack slot, then
  re-reads it with `cmp word [bp-6], 0 / je
  L_else`. A peephole could have skipped the
  store/reload and jumped directly on the `a < b`
  flags — BCC does not. None of `a, b, x` enregister
  here because each has only 1 syntactic use after
  initialisation, falling below the threshold.

## Call-crossing locals can only use SI/DI; 6th candidate spills

Fixtures `1508` (3 multi-use ints, one live across a
`call`), `1509` (6 multi-use ints, no calls), and
`1510` (4 multi-use ints, *all* live across a call)
all pass on the first capture and confirm the
hypothesis from [[batch-396-cx-pool]]: **locals whose
live range crosses a function call cannot use
DX/BX/CX — only the callee-saved SI/DI**.

- `1509`: 6 multi-use ints with no calls — first 5
  fit into SI/DI/DX/BX/CX, the 6th (`f`) spills to
  `[bp-2]`. So the maximum simultaneous in-register
  count without a call is exactly 5, consistent with
  the 5-register pool.
- `1508`: 3 multi-use ints with `c` used as arg and
  reassigned across `dbl(c)`. Result: `a` → DI, `c`
  → SI, **`b` → stack at `[bp-2]`** even though `b`
  is multi-use. Because all three locals are read
  again in the final `return`, all live across the
  call — but only 2 callee-saved regs are available.
  The middle local `b` is the one that loses out.
- `1510`: 4 multi-use ints all live across `dbl(d)`.
  Result: `a` → DI, `b` → `[bp-2]`, `c` → `[bp-4]`,
  `d` → SI. Only 2 enregistered, 2 spilled.

Updated register-allocation rule:
- **Without calls in the body**: pool is `{SI, DI,
  DX, BX, CX}` — 5 slots, declaration order.
- **With calls in the body**: locals that live
  across a call may only occupy `{SI, DI}` — 2
  slots. Locals whose live range does *not* cross
  the call may still claim DX/BX/CX.

Open question: when a local is the one passed as the
arg AND reassigned by the call return value (like
`c` in `1508` and `d` in `1510`), it appears to
preferentially get **SI** rather than DI — but both
batches have only a single such "call-target" local
to test against. The other in-register local takes
DI. Worth a 2-call-targets fixture to confirm.

## Enregistration extends to 5 regs: SI, DI, DX, BX, **CX**; fn-call ABI

Fixtures `1505` (5 multi-use ints all simultaneously
live), `1506` (2 multi-use ints with an intervening
function call), and `1507` (multi-use int paired
with a variable shift that needs CL) all pass on the
first capture.

- `1505` (**bigger finding**): 5 multi-use ints all
  enregister — into SI, DI, DX, BX, and **CX**. No
  stack allocation at all. So the enregistration
  pool spans all 5 general-purpose registers that
  aren't AX/BP/SP: `SI, DI, DX, BX, CX`. The order
  appears to be the declaration order of the locals.
- `1506` confirms the **caller-save / callee-save
  split for register-allocated locals**: across a
  `call _inc`, the locals in SI and DI are
  *not* spilled — BCC relies on SI/DI being callee-
  saved by the callee's `push si / push di`
  prologue. Arg cleanup uses `pop cx` (CX is scratch
  / caller-save and the simplest 2-byte reclaim).
  Function return comes back in AX; BCC then stores
  it into DI (the local's home register). This
  implies BCC will *not* place a local in DX, BX, or
  CX if its lifetime crosses a function call —
  otherwise the call would clobber it. (Hypothesis
  — needs a future probe with 3+ multi-use locals
  straddling a call.)
- `1507`: shift amount `n` is read only once
  syntactically, so it stays on the stack as
  expected. Notable detail: BCC loads it with `mov
  cl, [bp-2]` (`8a 4e fe`, byte load) rather than
  `mov cx, [bp-2]` (`8b 4e fe`) — same 3-byte length,
  but byte load is preferred when only `cl` is
  needed. The shift `sar ax, cl` follows immediately.

Updated register-allocation table:
| Order | Reg | Saved on entry? | Survives calls? |
|-------|-----|------------------|-----------------|
| 1     | SI  | `push si`        | yes             |
| 2     | DI  | `push di`        | yes             |
| 3     | DX  | not saved        | **no**          |
| 4     | BX  | not saved        | **no**          |
| 5     | CX  | not saved        | **no**          |

## Enregistration register order: SI, DI, DX, **BX** — 4 ints fit

Fixtures `1502` (4 locals, 2 multi-use + 2 single-
use), `1503` (4 locals **all** multi-use), and
`1504` (1 local with 4 syntactic uses) all pass on
the first capture and extend the enregistration
findings:

- `1502`: confirms the use-count rule under pressure
  — `a` and `b` (both used twice) go to SI/DI; `c`
  and `d` (both used once) stay on the stack at
  `[bp-2]` / `[bp-4]`. Prologue: `sub sp, 4` only,
  with `push si / push di`.
- `1503` (**major finding**): when 4 ints are all
  multi-use, all 4 go into registers — SI, DI, DX,
  and `**BX**`. No `sub sp` at all (no stack
  locals), and no `push bx` either — BCC treats BX
  as scratch in this calling convention and doesn't
  preserve it across the call from runtime startup.
  Code shape per assignment: `mov ax, REG / inc ax /
  mov REG, ax` (the inc-vs-add policy still applies
  to the AX temp).
- `1504`: 1 local with 4 syntactic uses → only SI
  needed; BX, DI, DX stay free. Each `v = v + K`
  round-trips through AX (`mov ax,si / op / mov
  si,ax`) — there's no peephole that keeps the
  result in AX and skips the store-back when the
  next use is also via AX.

Updated register-allocation table:
| Order | Reg | Saved on entry?  |
|-------|-----|------------------|
| 1     | SI  | `push si`        |
| 2     | DI  | `push di`        |
| 3     | DX  | not saved        |
| 4     | BX  | not saved        |

The first two (SI, DI) are pushed in the prologue.
DX and BX are treated as scratch — clobbered without
preservation. The maximum simultaneous enregistered
int count observed so far is 4.

## Enregistration heuristic narrowed: use-count threshold ≥ 2

Fixtures `1499` (`(a+b) + (a-c)` — `a` used twice),
`1500` (`while(a<b){c+=a; a++;}` — `a` and `c` each
used twice in the loop body+test), and `1501` (same
sum as `1496` but with declarations separated from
initialisers) all pass on the first capture and
together narrow the heuristic from
[[batch-393-enreg-spill]].

Observations:
- `1499`: only `a` (used twice in two distinct
  sub-expressions) goes to SI. `b` and `c` stay on
  the stack at `[bp-2]` / `[bp-4]`. The
  computation: `mov ax,si / add ax,[bp-2] / mov
  dx,si / sub dx,[bp-4] / add ax,dx`.
- `1500`: `a` → SI (read in cmp + written by `a++`),
  `c` → DI (compound `c += a` reads and writes), but
  `b` → `[bp-2]` (read once per cmp, syntactically
  one occurrence).
- `1501`: same lowering as `1496` — all on stack —
  confirming that *initialiser-at-declaration vs.
  initialiser-as-separate-statement* makes **no
  difference**. The init counts the same either way.

So the actual heuristic is: **enregister a local iff
it has ≥ 2 syntactic uses (read or write) after its
declaration, excluding the initialiser**. Each
syntactic operand counts once (e.g. `a < b` is one
read of `a` and one of `b`; `a++` is one use of `a`;
`c += a` is one use of `a` and one use of `c`).
Compound `+=` is one syntactic op even though
semantically it reads and writes — BCC counts it as
one. Under register pressure, the first ≥2-use
locals claim SI/DI/DX in declaration order; the
maximum simultaneous in-register count observed so
far is 3.

## Enregistration heuristic: 3/4/5-local pure sum all spills

Fixtures `1496` (`int a=1, b=2, c=3; return a+b+c;`),
`1497` (4 locals), and `1498` (5 locals) — all pure
"declare-with-literal-init then sum" — pass on the
first capture. Notable result: **all three fixtures
spill every local to the stack**. Code shape (for
3-local case):
`sub sp,6 / mov [bp-2],1 / mov [bp-4],2 /
mov [bp-6],3 / mov ax,[bp-2] / add ax,[bp-4] /
add ax,[bp-6]`. The 4-local and 5-local versions
just extend the pattern.

This contradicts the naive "BCC enregisters into SI,
DI, DX in order until full" model. The earlier
[[batch-392-char-idx-if-empty]] / fixture `1494`
showed 3 ints in SI/DI/DX, but `1494` differs from
`1496` in two ways: (a) its third local `int x;` had
no initializer at declaration — only a conditional
assignment in each arm of the if-else, and (b) `a`
and `b` are read *twice* each (cmp + sub) before
the return. So BCC's enregistration heuristic is
*not* purely positional — it depends on usage
density and/or initializer style. The "declared and
literal-initialised then read once" pattern of `1496`
falls below the enregistration threshold even at
just 3 locals.

Operational consequence: future fixtures that intend
to probe register-allocation should reference each
candidate local multiple times (e.g. in a compare or
loop) rather than a single sum, otherwise the locals
will silently fall to stack. The "single hot int
local with compound-op" pattern from
[[batch-390-rmw-non-ax]] is closer to the
enregistration sweet spot.

## char as arr idx, if-else with 3 locals enregistered, empty `void f()`

Fixtures `1493` (`int a[10]={0..9}; char c=3; return
a[c];` — signed char as int-array index), `1494`
(`int a=10, b=3; int x; if (a>b) x=a-b; else x=0;
return x;` — if-else with arith in both arms), and
`1495` (`void f(void){} int main(){f(); return 7;}` —
empty void function called from main) all pass on the
first capture. `1493` confirms signed-char-as-index
goes through `cbw`: `mov al,[bp-1] / cbw / shl ax,1
/ mov bx,ax / mov ax,[bx+_a]`. The char gets a 2-byte
stack slot (allocated by `dec sp / dec sp`) but only
the high byte `[bp-1]` holds the value — `[bp-2]` is
padding. BCC allocates a minimum 2-byte slot per
local even for a 1-byte type. `1494` shows BCC will
enregister *three* int locals when register pressure
allows: `a` → SI, `b` → DI, `x` → DX. DX is normally
a scratch register but BCC happily promotes a short-
lived local into it. The if-else lowers to `cmp si,
di / jle L_else / mov ax,si / sub ax,di / mov dx,ax
/ jmp / L_else: xor dx,dx / L_done: mov ax,dx`. The
`x = 0` arm becomes a one-cycle `xor dx,dx`. `1495`
confirms empty-body emission: `void f()` becomes
exactly 5 bytes — `55 8b ec 5d c3` (`push bp / mov
bp,sp / pop bp / ret`). The prologue is *not*
elided. `f` is still emitted as a PUBDEF. The call
site is `e8 disp16` with the standard near-relative
encoding/FIXUPP.

## Memory-dest RMW: `*p+=3`, `*p+=100`, `*p+=1000`

Fixtures `1490` (`*p += 3`), `1491` (`*p += 100`),
and `1492` (`*p += 1000`) all pass on the first
capture and complete the encoding-table calibration
started in the previous two batches. Here taking
`&v` forces `v` to memory (`[bp-2]`) and `p` is
enregistered into `SI`, so the compound add targets
`[si]` (ModR/M = `0x04` = mod=00 rm=100). Observed
encodings:
- `1490` (+3): `83 04 03` — opcode `0x83 /0`, mod=00
  rm=[si], imm8 sign-extended. 3 bytes.
- `1491` (+100): `83 04 64` — same opcode, imm8
  sign-ext (100 fits). 3 bytes.
- `1492` (+1000): `81 04 e8 03` — opcode `0x81 /0`,
  same ModR/M, imm16 follows. 4 bytes.

So the imm8-sign-ext vs imm16 boundary at [-128,127]
applies *identically* to memory and register
destinations of `add /0`. The only difference is the
ModR/M mode field (mod=11 for register, mod=00/01/10
for memory). Crucially, BCC never emits any small-add
unrolling for memory destinations — no `inc word
[si]` chain, even though `inc r/m16` (`FF 06 ...` for
[bp+disp] or `FF 04` for [si]) is one byte shorter
than `83 04 01`. The `inc`/`dec` optimization is
register-AX-only.

Other observations from these fixtures: prologue uses
`dec sp / dec sp` again to allocate the single 2-byte
`v` slot — confirms the pattern from
[[batch-384-2d-int-arr]]. `p` is set up via the
canonical `lea ax,[bp-2] / mov si, ax` two-step (not
`lea si, [bp-2]`).

## RMW non-AX add: `v+=3`, `v+=100`, `v+=1000` (imm8 vs imm16)

Fixtures `1487` (`v += 3`), `1488` (`v += 100`), and
`1489` (`v += 1000`) all pass on the first capture
and together calibrate the non-AX register-add
encoding. All three enregister `v` into `SI` (single
hot local), so the RMW is `add si, imm`, not `add
[bp+disp], imm` as the previous batch's note had
hypothesised. Observed encodings:
- `1487` (+3): `83 c6 03` — opcode `0x83 /0`, ModR/M
  `0xc6` = mod=11/rm=110(si), imm8 sign-extended. 3
  bytes.
- `1488` (+100): `83 c6 64` — same `0x83 /0` opcode,
  imm8 sign-extended (100 = 0x64 fits in
  -128..127). Still 3 bytes.
- `1489` (+1000): `81 c6 e8 03` — opcode `0x81 /0`,
  same ModR/M, imm16 follows (0x03e8 = 1000). 4
  bytes — imm doesn't fit in signed 8-bit.

So for non-AX register destinations the imm8-sign-
extended form `83 /0` is preferred for any value
that fits in [-128,127]; outside that range, BCC
falls back to `81 /0` with full imm16. This is
distinct from the AX-with-imm policy
([[batch-389-inc-dec-add]]), which canonicalises on
the `05` opcode for any `+N` with `N≥3`.

Correction to the previous batch's table: the
"non-AX destinations" row referred to `[bp+disp]`
memory operands, but the actual code path for a
single-local RMW promotes the local into SI and uses
the **register** form of `83 /0` instead. The
ModR/M's mod field distinguishes (mod=11 for
register, mod=00/01/10 for memory) but the imm8/16
boundary is the same.

## inc/dec-vs-add boundary: `v+3`, `v-1`, `v+100`

Fixtures `1484` (`int v=5; int r = v + 3; return r;`),
`1485` (`int v=5; int r = v - 1; return r;`), and
`1486` (`int v=5; int r = v + 100; return r;`)
together calibrate the boundary of the inc/dec-for-
small-add optimization first observed in [[batch-388-
arr-or-incpair]] / fixture `1483`. All pass on the
first capture. `1484` confirms `+3` uses `add ax, 3`
encoded as `05 03 00` (3-byte `add AX, imm16` form,
opcode `0x05`) — *not* three `inc ax`. So the
`inc`-chain optimization only applies to +1 and +2
(where 1 or 2 bytes is strictly smaller than the
3-byte `add` encoding); at +3 the byte counts tie and
BCC prefers the single `add` instruction. `1485`
confirms the symmetric path: `v - 1` lowers to `dec
ax` (opcode `0x48`, 1 byte) — BCC's small-sub path
mirrors small-add. `1486` confirms the AX-with-imm
encoding for non-tiny constants: `v + 100` is `05 64
00` (`add AX, imm16`), *not* the `83 c0 64` (`add
r/m16, imm8` sign-extended) alternative. The two
forms are both 3 bytes for AX; BCC's allocator
canonicalises on the `0x05` opcode whenever the
destination is AX. Summary of the integer-add encoding
table for AX:
- `+1`/`-1`: `40` / `48` (1 byte)
- `+2`/`-2`: `40 40` / `48 48` (2 bytes)
- `+N` for `N≥3`: `05 N N>>8` (3 bytes)
- non-AX destinations (e.g. `[bp+disp]`) use `83 /0
  disp imm8` when imm fits in 8 bits — different
  policy, since the imm8-sign-extended form is one
  byte shorter than imm16 for memory operands.

## `int x = a[0]+a[2]`, `int x = cmp || cmp`, `a[1] = v + 2`

Fixtures `1481` (`int a[3]={10,20,30}; int x = a[0]
+ a[2]; return x;` — int initializer from sum of two
constant-index array elements), `1482` (`int a=0,
b=5; int x = (a>0) || (b>0); return x;` — int
initializer from logical-OR of two compares), and
`1483` (`int a[3]; int v=5; a[1] = v + 2; return
a[1];` — store of `v + 2` expression to array
element) all pass on the first capture. `1481`
confirms folded-offset element access: `a[0]` reads
`[bp-6]`, `a[2]` reads `[bp-2]`, summed with `mov ax,
[bp-6] / add ax, [bp-2]`. The N_SCOPY@ helper still
runs for the brace initializer first (template
`0a 00 14 00 1e 00`). `1482` exposes the **``||``
short-circuit shape**, the mirror of [[batch-383-and-
not-for-fill]]'s `&&`: first compare uses a *non-
inverted* jcc that jumps **forward to the true path**
(`jg L_true`), then the second compare uses an
inverted jcc (`jle L_false`) to bail to false. Both
paths reconverge: `mov ax,1 / jmp L_done / xor ax,ax
/ L_done`. The asymmetry of `&&` vs. `||` lives in
which side gets the inverted vs. non-inverted jcc on
the first compare — `&&` uses inverted (skip-to-
false on fail), `||` uses non-inverted (skip-to-true
on first success). Returns 1 because b=5>0. `1483`
shows BCC's **inc-for-small-add** size optimization:
`v + 2` lowers to `mov ax,[bp-8] / inc ax / inc ax`
rather than `add ax, 2`. Two `inc ax` = 2 bytes (0x40
twice); `add ax, 2` would be 3 bytes (`83 c0 02` for
sign-extended imm8, or `05 02 00` for imm16). This is
a stable pattern — fixture `1057` (`x + 1`) emits the
same `inc ax` after the load. So integer adds of +1
or +2 use `inc` chains; +3 and larger fall back to
`add` (where the byte count ties or favors `add`).

## `arr[i].x` struct arr var idx, `int x = (a==b)`, `sizeof(*p)`

Fixtures `1478` (`struct S {int x;}; struct S arr[3];
int i=1; arr[i].x = 99; return arr[i].x;` — struct
array with variable index), `1479` (`int a=7, b=7;
int x = (a == b); return x;` — int initializer from
bare `==` compare), and `1480` (`int x=0; int *p =
&x; return sizeof(*p);` — sizeof of a dereferenced
pointer) all pass on the first capture. `1478`
confirms struct-array stride lowering: `sizeof(struct
S) = 2` (single int field) is a power of two, so the
scale is `mov bx,si / shl bx,1` (not `imul`) — same
pow2 rule that applies to `int` element strides. The
`.x` field offset is 0, so the LEDATA FIXUPP target
for `_arr` produces an effective `[bx+_arr+0]`, no
extra displacement add. Store and load both
recompute the scaled offset — no CSE. Returns 99.
`1479` matches the same boolean materialization
template as the earlier `<` and `&&` cases, but the
inverse jcc selected for `==` is `jne` (jump if not
equal): `mov ax,[a] / cmp ax,[b] / jne L_false / mov
ax,1 / jmp / xor ax,ax`. Result 1 since 7 == 7.
`1480` confirms that `sizeof(*p)` is a pure compile-
time fold: the deref is *not* evaluated at run time
— no `mov ax,[si]` is emitted. Only `int x = 0; int
*p = &x;` lower to real instructions (the unused-by-
value `p` is still spilled to `[bp-4]`); the return
becomes `mov ax, 2` directly. Confirms BCC honours
the C rule that the operand of sizeof is
unevaluated.

## stack `int a[3]={7}` partial, `char s[6]="hi"` stack, `(x>>4)&0xf`

Fixtures `1475` (`int a[3] = {7}; return a[0] + a[1]
+ a[2];` — stack int array partial brace init), `1476`
(`char s[6] = "hi"; return s[1];` — stack char array
initialized from string literal), and `1477` (`int x =
0x42; int y = (x >> 4) & 0xf; return y;` — nibble
extract via signed shift then AND mask) all pass on
the first capture. `1475` confirms partial brace init
for stack arrays goes through the `N_SCOPY@` 6-byte
memcpy helper: the initializer template is emitted in
`_DATA` as `07 00 00 00 00 00` (declared length 3 *
sizeof int = 6 bytes, padded with zeros for the
omitted elements), and runtime copies the full
template — there is no "init prefix then runtime
zero-fill the rest" split. Return path simply sums
[bp-6] + [bp-4] + [bp-2]. `1476` confirms the same
`N_SCOPY@` path for `char s[N] = "literal"` on the
stack: the template is `68 69 00 00 00 00` =
`"hi\0\0\0\0"` (the C-string terminator is included,
then zero-pad fills the rest of the declared length).
`s[1]` reads `[bp-5]` then `cbw` sign-extends `'i'`
(0x69, positive → 0x0069 = 105) for the int return.
`1477` confirms BCC does **not** fuse shift+mask into
a special nibble-extract or byte-extract pattern: `mov
ax,[bp-2] / mov cl,4 / sar ax,cl / and ax,0x000f /
mov [bp-4],ax`. Since the shift amount is K=4 (the
unroll threshold), BCC uses the `cl`-loaded variant
rather than unrolling. The `sar` (signed) is selected
because `x` is `int`. AND with literal uses the `ax,
imm16` short form (`25 0f 00`).

## stack-arr decay `f(a)`, `if (a[0]>a[1])`, `static int g[3]={...}`

Fixtures `1472` (`int sum(int *p) { return p[0] +
p[1]; } int main(void) { int a[2]; a[0]=3; a[1]=4;
return sum(a); }` — stack-array decay to int*
parameter), `1473` (`int a[3]; a[0]=5; a[1]=3;
a[2]=7; if (a[0] > a[1]) return 1; return 0;` —
neighbour int-element compare in `if`), and `1474`
(`static int g[3] = {7,8,9}; return g[1];` — static-
qualified global int array with brace initializer)
all pass on the first capture. `1472` confirms the
canonical decay shape for stack arrays: caller emits
`lea ax,[bp-4] / push ax / call _sum / pop cx` (one
`pop cx` for the 2-byte cdecl cleanup), callee reads
`mov si,[bp+4]` once and uses `mov ax,[si] / add ax,
[si+2]` for `p[0]` and `p[1]` — no shift for the
fixed index, just a literal +2 displacement in the
ModR/M. `1473` confirms `if (a[0] > a[1])` as a one-
sided branch: `mov ax,[bp-6] / cmp ax,[bp-4] / jle
L0 / mov ax,1 / jmp L1 / L0: xor ax,ax / L1:`. The
inverse jcc (`jle` for `>`) and the in-place `cmp
ax, m16` form are the standard pattern. Result is 1
since 5 > 3. `1474` confirms `static` global array
emission: the LEDATA holds `07 00 08 00 09 00` in
`_DATA`, but **no PUBDEF** is emitted for `g` — only
`_main` appears in the PUBDEF record. The load `mov
ax, [offset _g+2]` (with a LEDATA FIXUPP to the
private symbol) uses the segment-relative offset
directly. Static linkage = stripped from the public-
symbol table while staying in the data segment.

## `a[i][j]` both var idx, `int x = (a<b)`, `int *p; *p = K`

Fixtures `1469` (`int a[2][3]; int i=1, j=2; a[i][j]
= 7; return a[i][j];` — 2D global array with variable
indices on both dimensions), `1470` (`int a=3, b=7;
int x = (a < b); return x;` — int initializer from
single `<` compare), and `1471` (`int x=5; int *p =
&x; *p = 99; return x;` — write through a local int
pointer) all pass on the first capture. `1469`
confirms full 2D address arithmetic with no CSE: row
stride 6 (= 3 cols * 2 bytes) is computed as `mov
ax,si / mov dx,6 / imul dx` (so BCC uses `imul` for
non-pow2 row strides — does not decompose `*6` into
shifts), then `mov dx,di / shl dx,1` for the inner
index, `add ax,dx / mov bx,ax`, finally `mov
[bx+_a],7` with a LEDATA FIXUPP on the `_a` base. The
*identical* offset sequence is re-emitted verbatim
before the load — there is no common-subexpression
elimination across the store/load pair. `i` and `j`
enregister into SI and DI. `1470` confirms the same
boolean materialization template as [[batch-382-and-
not-for-fill]] but for a bare compare without `&&`:
`mov ax,[a] / cmp ax,[b] / jge L_false / mov ax,1 /
jmp L_done / xor ax,ax / L_done:`. The branch is
`jge` (signed not-less) — BCC emits the inverse
condition to skip the true side. `1471` confirms `*p
= K` lowering: `p` is enregistered into SI via the
canonical `lea ax,[bp-2] / mov si,ax` pair (BCC
routes the lea result through AX rather than
emitting `lea si,[bp-2]` directly — a known regalloc
inefficiency), then `*p = 99` becomes `mov [si], 99`
(ModR/M `04` = `[si]` indirect, imm16 follows). Stack
prologue uses `dec sp / dec sp` for the 2-byte `x`
slot — for a single int, the two-byte literal
decrement is preferred over `sub sp,2`.

## `int x = cmp && cmp`, `int x = !a`, `for (i;i<3;i++) a[i]=i`

Fixtures `1466` (`int a=1, b=2; int x = (a==1) &&
(b==2); return x;` — int initializer from logical-AND
of two equality compares), `1467` (`int a=5; int x =
!a; return x;` — int initializer from logical NOT),
and `1468` (`int a[3]; int i; for (i=0; i<3; i++)
a[i]=i; return a[2];` — for-loop writing through
indexed array store) all pass on the first capture.
`1466` confirms boolean materialization for `&&` into
an int slot: `cmp [bp-2],1 / jne L_false / cmp
[bp-4],2 / jne L_false / mov ax,1 / jmp L_done /
L_false: xor ax,ax / L_done: mov [bp-6], ax`. The
short-circuit emits two distinct jnes to a single
false label; the true path materialises 1 via `mov
ax,1` and the false path via `xor ax,ax`. `1467`
confirms the classic 8086 `!x` idiom: `mov ax,[a] /
neg ax / sbb ax,ax / inc ax`. `neg` sets CF when the
operand is nonzero, `sbb ax,ax` materialises -1 or 0
from CF, and `inc ax` flips it to 0 or 1. No
`test`/`jcc`/branch is emitted — the result is fully
data-flow. `1468` confirms the canonical for-loop
shape: `i` enregistered into SI, body lowered as `mov
bx,si / shl bx,1 / lea ax,[bp-6] / add bx,ax / mov
[bx],si`, with `inc si / cmp si,3 / jl body` for the
inc+test edge. The for-loop layout puts the test
*after* the body (`jmp test` precedes the body on
entry; `jl body` re-enters). `a[2]` returns as `[bp-
2]` — the trailing-element offset folds to a single
stack slot read.

## `x ^= x` self-XOR, `char *p = "Hi"; *p`, `a[1] += a[1]`

Fixtures `1463` (`int x=7; x ^= x; return x;` —
compound self-XOR), `1464` (`char *p = "Hi"; return
*p;` — local pointer to string literal then deref),
and `1465` (`int a[3]={1,2,3}; a[1] += a[1]; return
a[1];` — array element compound-add to itself) all
pass on the first capture. `1463` confirms BCC does
not fold self-XOR to zero at this opt level: `x` is
enregistered into SI, `mov si,7 / xor si,si` emits
the literal XOR before the return. The XOR is the
"r/m reg" form `33 f6` (xor si, si). `1464` confirms
local `char *p = "literal"` lowering: the string
"Hi\0" lives in `_DATA` (DGROUP), `p` is enregistered
into SI initialized by `mov si, offset Hi` with a
LEDATA fixup against DGROUP, then `mov al,[si] / cbw`
loads and sign-extends the first char (`'H'` = 72) for
the int-typed return. The pointer is never spilled to
the stack. `1465` confirms array-element self
compound-add: `a[1] += a[1]` lowers to `mov ax,
[bp-4] / add [bp-4], ax` — RHS loaded once into AX,
then `add r/m, r` performs the in-memory RMW with the
same operand. The initial `{1,2,3}` initializer is
copied to the stack via the standard `N_SCOPY@` six-
byte memcpy helper (push ss + lea offset + push ds +
push init-data offset + cx=6 + call). Final
`a[1] = 4`.

## `n %= 7; n /= 2`, `**pp += 3`, `s += a[i]` var idx

Fixtures `1460` (`int n=20; n %= 7; n /= 2; return n;`
— sequential mod-then-divide compound assigns), `1461`
(`int **pp = &p; **pp += 3; return x;` — compound `+=`
through a double-deref pointer-to-pointer), and `1462`
(`int a[3]={1,2,3}; int i=1; int s=10; s += a[i];
return s;` — int compound `+=` with array element via
runtime index) all pass on the first capture. `1460`
confirms two compound idiv operations on the same
slot: 20 mod 7 = 6, 6 / 2 = 3. Two `cwd / idiv` blocks
back-to-back. `1461` confirms RMW through pp: load
`p` from pp, then load slot via p, add 3, store back —
three address layers. x = 5+3 = 8. `1462` confirms
arr-elem-at-var-idx as compound RHS: `i` is scaled by
2 (int stride), added to `_a`, byte-loaded into AX,
then added into s. Result 10+2 = 12.

## `a[0] == a[2]` char elem-elem, global arr `g[1] = v`, nested ternary

Fixtures `1457` (`char a[3]; a[0]='X'; a[2]='X'; if
(a[0] == a[2]) return 1;` — equality between two char-
array elements), `1458` (`int g[3]; int v=42; g[1] =
v; return g[1];` — store an int var into a global-
array element), and `1459` (`int a=5; a += b > c ? 10
: b < c ? 20 : 0; return a;` — int compound `+=` with
nested ternary RHS) all pass on the first capture.
`1457` confirms char-array element pair comparison:
both load with `cbw`, then `cmp ax,dx / je TRUE`.
With a[0]=a[2]='X', returns 1. `1458` confirms global
arr store: var loaded into AX, then `mov [_g+2],ax`
for index 1 (offset 2 bytes for int). `1459` is the
two-level ternary in compound `+=`: outer test `b>c`
is false → fall to inner ternary `b<c ? 20 : 0` →
true → 20. a += 20 = 25.

## `if (c != 0)` char, `a -= ?: ternary RMW`, `a*b + c` fn

Fixtures `1454` (`char c=5; if (c != 0) return 1;` —
char compared to zero with explicit `!=`), `1455`
(`int a=5; int b=3; a -= a < b ? 0 : a - b; return a;`
— int compound `-=` where the RHS is a ternary
involving the same LHS), and `1456` (`int sum(int a,
int b, int c) { return a*b + c; } sum(2,3,4);` — fn
combining mul-then-add with three int args) all pass
on the first capture. `1454` confirms `c != 0`
lowers identically to using the char as a truthiness
test: `mov al,[bp-c] / cbw / or ax,ax / je FALSE`
(maybe with `cmp` instead of `or` due to the
explicit form). `1455` shows the ternary computes
into AX, then `sub word ptr [bp-a],ax`. a=5,b=3:
`a<b` false → use `a-b` (=2) → a -= 2 = 3. So a
becomes the min of a,b. `1456` confirms 3-arg
fn with mul+add body: 2*3+4 = 10.

## `c(b(a(x)))` three-fn chain, nested while 2x2, `a -= b[1]`

Fixtures `1451` (`int a(int x) { return x+1; } int b
(int x) { return a(x)+1; } int c(int x) { return b(x)+
1; } return c(5);` — three-level function-call chain
where each fn adds 1), `1452` (`int i=0; while (i<2) {
j=0; while (j<2) { s++; j++; } i++; }` — nested
while-loops counting iterations 2x2), and `1453`
(`int a=20; int b[2]; b[1]=3; a -= b[1]; return a;` —
int compound `-=` with stack-array element RHS) all
pass on the first capture. `1451` confirms the call
chain through three frames: `c(5)` pushes 5 into its
frame, calls b, b calls a, a returns 6, b returns 7,
c returns 8 — each fn just adds 1 to its arg. `1452`
confirms nested-while frame management: outer test
+body+inc share `i`, inner test+body+inc share `j`,
with `j=0` re-init each outer iteration. Total s = 4.
`1453` confirms the stack-array elem RHS counterpart
to `1336`'s `+=`: `mov ax,[bp-base+2] / sub word ptr
[bp-a],ax`. Result 20-3 = 17.

## `switch (n % 3)`, struct with int-array field, `a + 'A'`

Fixtures `1448` (`int classify(int n) { switch (n %
3) { ... } } return classify(7);` — switch dispatching
on a modulo expression), `1449` (`struct S { int v[3];
}; struct S s = {{1,2,3}}; return s.v[0] + s.v[2];` —
struct whose only field is an int array, with brace-
nested init), and `1450` (`int a = 5; return a + 'A';`
— int sum with a char literal RHS) all pass on the
first capture. `1448` confirms switch on expression:
`n % 3` computes into AX first (via idiv), then the
dense small-switch dispatch uses AX. 7%3=1 → 200.
`1449` confirms struct-with-int-array layout: the
struct takes the same space as the bare `int v[3]`
(6 bytes), and `s.v[0]` etc. compute offsets through
the struct first then the array. Sum 1+3 = 4. `1450`
confirms char literal in int arith: `'A'` folds to 65
at parse time, so we see `mov ax,[bp-a] / add ax,65`.
Result 70.

## `min3(5,3,8)`, fn with local int array, `a[0] ^= a[1]`

Fixtures `1445` (`int min3(int a, int b, int c) { int
m=a; if (b<m) m=b; if (c<m) m=c; return m; } min3(5,3,
8);` — min-of-three via cascading if), `1446` (`int
sum_local(void) { int a[3]; ... return a[0]+a[1]+a[2];
}` — function with a local int array on its own
stack), and `1447` (`char a[2]; a[0]=0xff; a[1]=0x0f;
a[0] ^= a[1]; return a[0];` — char array element
compound XOR with another array element) all pass on
the first capture. `1445` confirms the classic min3
shape: each cmp/if-update sequence runs in order; m
ends with min. Result 3. `1446` confirms callee-stack
array allocation: 3 ints in `a` = 6 bytes added to
the frame, populated in-line, then summed. Sum 6.
`1447` confirms char-arr-elem `^=` with arr-elem RHS:
load `a[1]` byte → cbw → AX = 0x0F, XOR with
`a[0]` byte loaded, narrow store back to a[0].
Result 0xFF ^ 0x0F = 0xF0 = 240 (signed view: -16).

## Array-of-struct init, `add5(a[1])`, `a[i] = i * 10`

Fixtures `1442` (`struct P arr[2] = {{1,2}, {3,4}};
return arr[1].x + arr[0].y;` — global array-of-struct
with nested init list), `1443` (`int add5(int x) {
return x + 5; } a[1]=10; return add5(a[1]);` —
function call passing an array element by value), and
`1444` (`for(i=0;i<3;i++) a[i] = i * 10; return a[2];`
— array fill using a multiplication of the loop index)
all pass on the first capture. `1442` confirms array-
of-struct global init: four ints laid out contiguously
in the data segment, each `{x,y}` pair occupying 4
bytes. `arr[1].x` = 3, `arr[0].y` = 2. Total 5. `1443`
confirms passing an array elem by value: `mov ax,
[bp-base+2] / push ax / call _add5`. Result 10+5 = 15.
`1444` confirms loop-driven array fill with index-mul
RHS: each iteration computes `i * 10` into AX, then
stores into the indexed slot via a separate base+
offset address calc. a[2] = 20.

## `zero(arr, 3)` mutating fn, sequential `for` loops, `a += two() + 3`

Fixtures `1439` (`void zero(int *a, int n) { ... a[i]
= 0; ... } int arr[3] = {1,2,3}; zero(arr, 3); return
arr[1];` — function that zeroes an int array via
pointer arg), `1440` (`for(i=0;i<3;i++) s+=i; for(i=
0;i<2;i++) s+=10; return s;` — two sequential for-
loops in the same function body), and `1441` (`int a=
5; a += two() + 3; return a;` — int compound `+=`
with `call() + const` RHS) all pass on the first
capture. `1439` confirms array-mutation via fn-ptr-
arg: caller passes `arr` (decay), callee writes 0
through `a[i]`. After the call arr[1] = 0. `1440`
confirms two sequential loops emit two independent
test/body/step blocks — they share the `i` slot but
each has its own labels. Final s = (0+1+2) + (10+10)
= 23. `1441` confirms compound RHS combining a call
and a const: call → AX = 2, `add ax,3` = 5, then
`add word ptr [bp-a],ax`. Result 5+5 = 10.

## `char c %= 4`, five-local sum, `-a[1]` neg of arr elem

Fixtures `1436` (`char c=17; c %= 4; return c;` —
char compound `%=` with a power-of-2 const), `1437`
(`int a=1; b=2; c=3; d=4; e=5; return a+b+c+d+e;` —
function with five int locals summed), and `1438`
(`int a[3]; a[1]=5; return -a[1];` — unary minus
applied to an array element load) all pass on the
first capture. `1436` confirms `%=` for char with
pow2 const goes through the usual `cwd / idiv` path
(no shift-and shortcut for signed mod, per `1263`).
17 mod 4 = 1. `1437` confirms 5-slot frame growth:
each local is one word in the stack frame
(`SUB SP, 10`), then five independent stores from
immediates, then chained adds for the return. Sum
1+2+3+4+5 = 15. `1438` confirms `neg` of array
element: load `a[1]` into AX, `neg ax`, return.
Result -5 → exit_code 251.

## `do { } while (0)`, `if ((a = b))`, chained 4-arm ternary

Fixtures `1433` (`int n=0; do { n++; } while (0);
return n;` — do-while with a constant-zero condition,
exercising the at-least-once semantic), `1434` (`int
a; int b=5; if ((a = b)) return a; return 0;` —
if-condition that contains an assignment, using the
assigned value as the truthy test), and `1435` (`return
a==0 ? 100 : a==1 ? 200 : a==2 ? 300 : 0;` — four-arm
chained ternary as the return value) all pass on the
first capture. `1433` confirms the do-while runs the
body once regardless of the test: n increments to 1,
then `cmp ...,0 / jne TOP` falls through. The
constant-folded `0` may or may not get short-circuited
to a hardcoded exit — the OBJ match shows BCC's
actual choice. `1434` confirms assign-in-if-cond: AX
gets the assigned value (5), `or ax,ax / je FALSE`.
`1435` confirms the right-associative ternary chain:
each `?:` is its own decision point, with the false
arm cascading to the next test. Result 300.

## `char c += a*2`, identical-literal ptr eq, `s.x + a[1]`

Fixtures `1430` (`int a=5; char c=10; c += a * 2;
return c;` — char compound `+=` with int-mul RHS),
`1431` (`char *p = "abc"; char *q = "abc"; if (p == q)
return 1; return 0;` — equality between two pointers
that each point to the same string literal text), and
`1432` (`struct S { int x; }; struct S s={3}; int a[2]
={5,7}; return s.x + a[1];` — sum of a struct-field
load and an array-elem load) all pass on the first
capture. `1430` confirms the char-`+=`-int-result
shape: mul `a * 2` computes into AX (=10), then byte-
narrow-add into c's slot. Result 10+10 = 20. `1431`
confirms BCC behavior on duplicated string literals:
both `"abc"` references can either share storage
(literal pool dedup) or be separate -- the OBJ
match shows whatever BCC actually does, and the
return value reveals whether they're pooled. `1432`
is the cross-aggregate sum: each load reads from a
different global, both add into AX. 3+7 = 10.
Process note: 1430's first verify hung in DOSBox
(another flaky audio init); single retry passed.

## `if (x >= 0)`, `a[char i]`, global `gp->x = 42`

Fixtures `1427` (`int isnneg(int x) { if (x >= 0)
return 1; return 0; } return isnneg(-5);` — non-
negative check via `>=`), `1428` (`char a[5]; char i=
'\002'; return a[i];` — array subscript using a char
variable as the index), and `1429` (`struct S *gp =
&g; gp->x = 42; return gp->x;` — global struct
pointer initialized to global, then used for read-
write through arrow field) all pass on the first
capture. `1427` confirms `>=` lowers as the negation
of `<`: `cmp ax,0 / jl FALSE` shape — equivalent to
the existing `<` and `>` infrastructure. isnneg(-5)
= 0. `1428` confirms char-as-index `cbw`-promotes
to int for the address calculation: `mov al,[bp-i]
/ cbw / mov bx,ax / mov al,[bx+...]`. Result 30.
`1429` confirms global ptr-to-struct init from
another global's address: gp's data record holds the
OFFSET of g, then arrow access goes through the
pointer. Returns 42.

## `*p = five()` deref-store call, iterative fib, char arr copy loop

Fixtures `1424` (`int five(void){return 5;} *p =
five(); return x;` — store function-call result
through pointer dereference), `1425` (`int a=0, b=1;
for (i=0;i<5;i++) { t=a+b; a=b; b=t; } return a;` —
iterative Fibonacci via three-variable rolling
update), and `1426` (`char src[3]="ab"; char dst[3];
for(i=0;i<3;i++) dst[i] = src[i]; return dst[1];` —
copy char-array elements via indexed loop) all pass
on the first capture. `1424` confirms call-as-RHS of
deref store: call lands in AX, then `mov bx,[bp-p] /
mov word ptr [bx],ax`. `1425` runs five Fibonacci
iterations: (0,1)→(1,1)→(1,2)→(2,3)→(3,5)→(5,8).
Return a = 5. The three-var shuffle `t=a+b; a=b; b=
t;` requires three memory loads + stores per
iteration; no register-allocation fusion. `1426`
confirms global char-arr to global char-arr copy:
loaded byte-by-byte through `mov al,[bx+_src] / mov
[bx+_dst],al`. dst[1] = 'b' = 98.

## `*p = *p + 1`, `-(-10)`, `a >>= 2; a <<= 1;`

Fixtures `1421` (`*p = *p + 1; return a;` — read-
modify-write through pointer using an explicit add
rather than compound), `1422` (`int a = -10; return -
a;` — unary minus on a negative-initialized variable),
and `1423` (`int a=8; a >>= 2; a <<= 1; return a;` —
sequential right-shift then left-shift on same local)
all pass on the first capture. `1421` confirms the
non-compound RMW path: load `*p` into AX (=5), add
1 (AX=6), store back through `*p`. Result a = 6.
This is the un-fused counterpart to a `(*p)++` --
explicit add doesn't get the compound-inc shortcut.
`1422` confirms `-a` on a negative-init var: -(-10)
= 10, which is the standard `neg ax` after load.
`1423` is two sequential compound shifts: `a >>= 2`
folds to two unrolled `shr ax,1` (K<=3 threshold per
batch 290), then `a <<= 1` similarly. 8>>2 = 2,
then 2<<1 = 4.

## `v = a[1]++`, linked-node `a.next->v`, `sumC` char arr

Fixtures `1418` (`int a[3]; ... v = a[1]++; return
a[1]*10 + v;` — post-increment of an array element
captured into another local), `1419` (`struct N { int
v; struct N *next; }; struct N b={2,0}; struct N a=
{1,&b}; return a.next->v;` — global struct chained via
pointer field), and `1420` (`int sumC(char *s, int n)
{ ... t += s[i]; ... } char a[3]={1,2,3}; return
sumC(a, 3);` — sum of char-array elements through fn
arg) all pass on the first capture. `1418` confirms
post-inc on array element: load a[1] (=20) into AX,
v = 20, then `inc word ptr [bp-base+2]` makes a[1]=
21. Return 21*10+20 = 230. `1419` confirms struct
init with cross-struct pointer reference (`&b` in
a's initializer at file scope): the global init
record holds the OFFSET to b, then `a.next->v` does
ptr-load then field-load. Result = b.v = 2. `1420`
confirms char-array passed as char*: callee indexes
`s[i]`, byte-loads, `cbw`-promotes, adds. 1+2+3 = 6.

## Popcount, min function, `c = a[1]` char arr elem

Fixtures `1415` (`int popcount(int x) { int c=0;
while (x) { if (x&1) c++; x >>= 1; } return c; }
return popcount(0x55);` — popcount via bit-scan
loop), `1416` (`int min(int a, int b) { if (a < b)
return a; return b; }` — minimum-of-two function),
and `1417` (`char a[3]; ... c = a[1]; return c;` —
char local init from char-array element) all pass
on the first capture. `1415` confirms a real-world
bit-counting loop: `while (x)` tests against 0, `if
(x & 1)` selects the low bit, `x >>= 1` shifts. For
x = 0x55 = 01010101, four bits set → return 4.
`1416` is the canonical min function; trivial
control flow. `1417` confirms char-from-arr-elem
init: load byte at `[bp-base+1]`, store byte at
`[bp-c]`. Result 'Y' = 89. (1417 hit a transient
DOSBox PulseAudio crash on verify; passed on retry.)

## `a[0] * a[2]`, `for (; *p; p++)`, `**pp = 42`

Fixtures `1412` (`int a[3] = {2,3,4}; return a[0] *
a[2];` — multiply of two global-array elements at
const indices), `1413` (`for (; *p; p++) n++;` —
for-loop with empty init, deref condition, and
pointer step), and `1414` (`int *p=&x; int **pp=&p;
**pp = 42; return x;` — store through a double-deref
pointer-to-pointer) all pass on the first capture.
`1412` confirms two-elem mul: `mov ax,[_a+0] / imul
word ptr [_a+4]`. Result 2*4 = 8. `1413` is the for-
loop equivalent of `1267`'s while-based strlen: the
init is empty (no first-iter setup), the test is
`*p`, the step is `p++`. Length of "ab" = 2. `1414`
confirms write-through-pp: `mov bx,[bp-pp] / mov bx,
[bx] / mov word ptr [bx],42`. So x gets 42 set
through two indirections, then `return x` reads back
42.

## Sum-of-squares, `char *p += 1`, iterative factorial

Fixtures `1409` (`for (i=1; i<=4; i++) s += i * i;
return s;` — sum-of-squares accumulator), `1410`
(`char *p = "abc"; p += 1; return *p;` — char pointer
compound-add by 1, then deref), and `1411` (`int r=
1; for (i=1; i<=4; i++) r *= i; return r;` —
iterative factorial via `*=` accumulator) all pass on
the first capture. `1409` is a standard arith-in-loop
pattern: each iteration `i * i` computes the square
(via stack-spill mul of i with itself), then `+= s`.
Sum 1+4+9+16 = 30. `1410` confirms char-ptr += const:
`add word ptr [bp-p],1` (char-stride 1, immediate
folded). Then `mov bx,[bp-p] / mov al,[bx] / cbw`
reads 'b' = 98. `1411` is the iterative counterpart
to `1220`'s recursive factorial: the loop variable
multiplies into `r`. 1*1*2*3*4 = 24.

## `a[1] == x` char vs int, sequential `+=/-=`, `countLen("hello")`

Fixtures `1406` (`char a[3]; int x=5; ... if (a[1] ==
x) return 1;` — char-array element compared to int
variable in if-cond), `1407` (`int a=5; a += 10; a -=
3; return a;` — sequential compound `+=` then `-=` on
the same local), and `1408` (`int countLen(char *s) {
int n=0; while (*s != 0) { n++; s++; } return n; }
return countLen("hello");` — explicit-null-cmp
strlen-style function call) all pass on the first
capture. `1406` confirms char-int compare promotes
char via `cbw`: load `a[1]` byte → `cbw` → cmp to x
slot. With a[1]=5 and x=5, returns 1. `1407` confirms
two compound statements on the same lvalue emit two
independent in-place memory ops: `add word ptr [bp-
a],10 / sub word ptr [bp-a],3`. Result 5+10-3 = 12.
`1408` is the explicit-null variant of `1267`'s
strlen (`while (*s)` implicit). The `!= 0` doesn't
change codegen since BCC already lowers `while (*s)`
as `cmp byte ptr [bx],0 / je END`. Length 5.

## `char getc()` return, `a |= b[0]`, `compute(5)` multi-stmt

Fixtures `1403` (`char getc(void) { return 'X'; }
return getc();` — char-returning function called and
returned as int), `1404` (`int b[2] = {0x0a, 0x05};
int a=0xf0; a |= b[0]; return a;` — int compound `|=`
with a global int-array element RHS), and `1405`
(`int compute(int x) { int t = x + 1; t = t * 2;
return t; } return compute(5);` — multi-statement
function body with intermediate temp) all pass on the
first capture. `1403` confirms char-returning fn:
callee writes `'X'` (0x58) into AL, the AH bits are
undefined per ABI, but main reads the *int* AX so the
caller sees whatever AH happened to be -- BCC always
writes a sign-extended int via `mov al,88 / cbw`
(or similar) so the result is consistent. Final 88.
`1404` confirms `|=` with global-arr-elem RHS: `mov
ax,[_b+0] / or word ptr [bp-a],ax`. Result 0xF0 |
0x0A = 0xFA = 250. `1405` confirms multi-stmt fn
body: each stmt lowers independently, the temp `t`
lives in a slot, ultimately returned via AX.
(5+1)*2 = 12.

## `uchar + uchar` over 255, swap via struct ptrs, `a -= two()`

Fixtures `1400` (`unsigned char a=200; unsigned char b=
100; return a + b;` — sum of two unsigned chars whose
arithmetic result exceeds 255), `1401` (`void swap
(struct S *a, struct S *b) { int t=a->x; a->x=b->x;
b->x=t; }` — swap struct fields through two struct
pointers), and `1402` (`int two(void) { return 2; }
int a=10; a -= two(); return a;` — int compound `-=`
with function-call result as RHS) all pass on the
first capture. `1400` confirms uchar arithmetic: each
uchar zero-extends to int via `xor ah,ah` (or `mov
al,...` then implicit zero in ah), 200+100=300. Since
return type is int, the 300 carries through without
truncation. Exit-code low byte is 44 (300 mod 256).
`1401` is the struct-ptr counterpart to `1274`'s int-
ptr swap: same shape but the deref reads/writes use
the `->x` field offset. After swap, s1.x=7. `1402`
confirms `-=` with call result: call lands in AX,
then `sub word ptr [bp-a],ax`. 10-2 = 8.

## `while (next() < 3)`, `arr[0] + arr[2]`, `s += (int)a[i]`

Fixtures `1397` (`int next(void) { x++; return x; }
while (next() < 3) ;` — while-loop whose condition is
a function-call result, with the function mutating
external state), `1398` (`char arr[3]; arr[0]='A';
arr[1]='B'; arr[2]='C'; return arr[0] + arr[2];` —
sum of two char-array elements returned as int), and
`1399` (`char a[4] = {1,2,3,4}; for (i=0;i<4;i++) s
+= (int)a[i]; return s;` — sum of char-array elements
with explicit `(int)` cast on each elem) all pass on
the first capture. `1397` confirms call-as-cond
inside a while-loop: each iteration calls `_next`,
result in AX, `cmp ax,3 / jge END`. Side effects in
`next` (`x++`) accumulate across iterations. Loop
exits when x reaches 3, returns 3. `1398` is the
double-element variant of `1342`: each elem `cbw`-
promotes to int, then sum into AX. `1399` confirms
explicit `(int)a[i]` cast: same `cbw` lowering as
implicit promotion, no additional cast machinery --
the cast is a no-op at the OBJ level when the
underlying load already produces an int via cbw.

## `char *names[3]`, `(a==b) == (b<c)`, 4-way `if/else if/else`

Fixtures `1394` (`char *names[3] = {"hi", "ab", "x"};
return names[0][1];` — array of char-pointer init with
three string literals, then double-subscript), `1395`
(`if ((a == b) == (b < c)) return 1;` — equality
between two comparison results), and `1396` (`if (a==0)
return 0; else if (a==1) return 1; else if (a==2)
return 2; else return 3;` — four-way if-else-if chain)
all pass on the first capture. `1394` confirms global
array-of-pointers init: each pointer slot is initialized
with the address of its corresponding string literal,
laid out in the data segment. `names[0][1]` does two
deref-and-load: first `names[0]` = ptr to "hi",
second `[1]` = 'i' = 105. `1395` confirms compare-as-
int composed: each inner cmp materializes to 0 or 1
via sete-style boolean materialization, then the outer
`==` compares two int 0/1 values. Both inner are true
(1==1), so outer is true → return 1. `1396` extends
`1201`'s three-way pattern: each `else if` chains
through the same false-jump target, accumulating until
the final `else` catches the unmatched case. With a=2
the third arm fires.

## `gcd(12,8)` recursive, `char ^= 0xff`, `a %= b*c`

Fixtures `1391` (`int gcd(int a, int b) { if (b==0)
return a; return gcd(b, a % b); } return gcd(12, 8);`
— recursive GCD via Euclidean algorithm), `1392`
(`char c=0x55; c ^= 0xff; return c;` — char compound
XOR with high-byte mask const), and `1393` (`int a=
20; int b=3; int c=2; a %= b * c; return a;` — int
compound `%=` with a product RHS) all pass on the
first capture. `1391` confirms recursion through
two distinct args + modulo expression as the
recursive arg: gcd(12,8) → gcd(8,4) → gcd(4,0) → 4.
`1392` confirms `^=` with byte const: `xor byte ptr
[bp-c],0FFh`. Result 0x55 ^ 0xFF = 0xAA = 170 as
unsigned, -86 as signed. `1393` confirms `%=` with
product RHS: `b * c` into AX (=6), push, load a,
cwd, idiv -- remainder back to a. 20 mod 6 = 2.

## Int local `*= char`, `a += (b+c)`, `a *= (b+c)`

Fixtures `1388` (`int a=2; char c=3; a *= c; return a;`
— int local compound `*=` with a char RHS), `1389`
(`int a=5; int b=3; int c=2; a += (b + c); return a;`
— int compound `+=` with parenthesized sum RHS), and
`1390` (`int a=2; int b=3; int c=4; a *= (b + c);
return a;` — int compound `*=` with parenthesized sum
RHS) all pass on the first capture. `1388` is the
local counterpart to `796`'s global int *= char: char
`cbw`-promoted into AX, then `imul word ptr [bp-a]`
back into a. Result 2*3 = 6. `1389` confirms paren-
sum-RHS for `+=`: `b + c` computed into AX (=5), then
`add word ptr [bp-a],ax`. Total 5+5 = 10. `1390`
mirrors `1389` for `*=`: `b + c` into AX (=7), then
imul against [bp-a]. Result 2*7 = 14.

## `sum(arr, 3)` array via ptr, `char a[5] = "ab"`, swap elems

Fixtures `1385` (`int sum(int *a, int n) { ... for
(i=0;i<n;i++) s += a[i]; return s; } return sum(arr,
3);` — sum function taking an int* pointer and length),
`1386` (`char a[5] = "ab"; return a[3];` — global char
array sized larger than the string-literal init), and
`1387` (`int a[2]; t=a[0]; a[0]=a[1]; a[1]=t; return
a[0];` — three-statement swap of two array elements
through a temp) all pass on the first capture (after a
single transient DOSBox hang on 1385's initial capture
that required killing the stuck process and retrying).
`1385` confirms array-as-ptr argument + loop sum:
caller passes `arr` (base address) and `3`, callee
indexes via `a[i]`. Sum 1+2+3 = 6. `1386` confirms
oversized char-array partial string init: "ab\0"
fills the first 3 bytes, remaining 2 zero-fill in the
data segment record. `a[3]` returns 0. `1387` confirms
the classic temp-swap idiom emits three independent
load-store sequences with no fusion/elision -- just
six word moves.

**Process note**: batch 356 hit another DOSBox hang
(third in this session, all on different fixtures);
kill + retry succeeded each time. The host's PulseAudio
init is unreliable.

## while-inside-for, `a |= s.x`, `c = (char)(a + 100)`

Fixtures `1382` (`for(i=0;i<3;i++) { j=i; while (j>0)
{ s++; j--; } } return s;` — while loop nested inside
a for loop), `1383` (`struct S {int x;}; struct S
s = {0x0f}; ... a |= s.x; return a;` — int compound
`|=` with a struct-field RHS), and `1384` (`int a=5;
char c; c = (char)(a + 100); return c;` — char
narrow-cast applied to a parenthesized sum) all pass
on the first capture. `1382` confirms a different
nested-loop shape from `1369`'s nested-for: outer
post-step (`i++`) and an inner condition-driven
loop (`while (j > 0)`). Each i iteration does i
increments of s. Total s = 0+1+2 = 3. `1383`
confirms struct-field RHS for `|=`: `mov ax,[_s+0] /
or word ptr [bp-a],ax`. Result 0xF0 | 0x0F = 0xFF =
255. `1384` confirms cast-on-paren-expr: `a + 100`
computes into AX (=105), then `(char)` narrows on
store: `mov byte ptr [bp-c],al`. 105 fits in signed-
byte range, so no truncation.

## `*(a + i)`, `if (!f())`, `a += b >> 1`

Fixtures `1379` (`int a[3]; int i=1; return *(a + i);`
— deref of pointer-plus-variable from array base
directly), `1380` (`int f(void) { return 0; } if (!f())
return 1;` — if-condition using logical-not on a call
result), and `1381` (`int a=10; int b=4; a += b >> 1;
return a;` — int compound `+=` with shift-expression
RHS) all pass on the first capture. `1379` confirms
`*(a + i)` decays the array name to a pointer, adds
scaled index, then dereferences -- same lowering as
`a[i]` but written through pointer syntax. Result 20.
`1380` confirms `!f()`: call result lands in AX, `or
ax,ax / je TRUE_BRANCH` shape (inverted) -- the
if-cond's polarity flips so a *zero* call result is
the "true" case. `1381` confirms shift-expr-as-RHS
of compound: `b >> 1` computes into AX first (b=4
shifts to 2), then `add word ptr [bp-a],ax`. Result
10+2 = 12.

## `int n = 1 << 15`, `char c = 'a' + 1`, `a += (a+1, 2)`

Fixtures `1376` (`int n = 1 << 15; return n;` — int
init from a shift that overflows signed int range),
`1377` (`char c = 'a' + 1; return c;` — char init
from char-literal-plus-int arithmetic), and `1378`
(`int a=5; a += (a + 1, 2); return a;` — int compound
`+=` whose RHS is a comma expression discarding an
expression involving the LHS) all pass on the first
capture. `1376` confirms the constant folder evaluates
`1 << 15 = 32768`, which doesn't fit in signed int but
just becomes 0x8000 = -32768 as the bit pattern. Init
emits `mov word ptr [bp-n], 8000h`. Return value is
-32768; exit-code interpretation depends on shell
(low byte = 0). `1377` confirms char arith fold:
`'a' + 1` = 97+1 = 98, init becomes `mov byte ptr [bp-
c],62h`. `1378` confirms comma-as-compound-RHS: LHS
`a+1` is evaluated for side effect (none here, value
discarded), RHS `2` becomes the comma value, then
outer `+=` adds 2 to a. Result a = 5+2 = 7.

## Recursive `rpow(2,5)`, `a /= b[0]`, `buf[0] | buf[1]`

Fixtures `1373` (`int rpow(int b, int e) { if (e==0)
return 1; return b * rpow(b, e-1); } return rpow(2,
5);` — recursive power function), `1374` (`int a=20;
int b[2]; b[0]=4; a /= b[0]; return a;` — int local
compound `/=` with a stack-array element RHS), and
`1375` (`char buf[3]; buf[0]=0x30; buf[1]=0x05;
return buf[0] | buf[1];` — OR of two char-array
elements returned as int) all pass on the first
capture. `1373` confirms recursion w/ mul-after-call:
5 recursive frames before the base case (e==0 returns
1), then unwind multiplying by `b` each frame. 2^5=
32. `1374` confirms array-elem-RHS compound `/=`:
load `b[0]` into AX, push, load `a` into AX, cwd,
idiv [sp+0], result back to a. 20/4 = 5. `1375`
confirms two char-arr elem OR: each elem byte-loads,
`cbw`-promotes to int, OR'd in AX. 0x30 | 0x05 = 0x35
= 53.

## `a += b>0 ? 10 : -10`, char arr elem `+=`, `a[idx()]`

Fixtures `1370` (`int a=5; int b=3; a += b > 0 ? 10 :
-10; return a;` — int compound `+=` whose RHS is a
ternary), `1371` (`char a[3]; a[1] = 20; a[1] += 5;
return a[1];` — char-array element compound `+=` with
a const), and `1372` (`int idx(void){return 1;} ...
return a[idx()];` — array subscript using a function-
call result as the index) all pass on the first
capture. `1370` confirms ternary RHS materializes
into AX before the compound add: arms write `10` or
`-10` and join, then `add word ptr [bp-a],ax`.
Result 5+10 = 15. `1371` confirms char-arr-elem
compound `+=`: load `a[1]` byte → cbw → add → narrow
store. Or: `add byte ptr [bx+_a+1],5` directly with a
const index. Either way: 20+5 = 25. `1372` confirms
call result as subscript: the call returns 1 in AX,
then `shl ax,1 / add ax, offset _a / mov ax,[ax]`
loads a[1] = 20.

## `do { i--; } while (i > 0)`, `while (i--)`, nested `for s += i*j`

Fixtures `1367` (`int i=5; do { i--; } while (i > 0);
return i;` — do-while with post-decrement body, signed
test against 0), `1368` (`int i=10; int s=0; while (i--
) s++; return s;` — while loop whose condition is a
post-decrement (the classic count-down idiom)), and
`1369` (`for(i=0;i<3;i++) for(j=0;j<2;j++) s += i*j;
return s;` — nested for-loop summing index products)
all pass on the first capture. `1367` decrements i
five times from 5→0, exits when i==0, returns 0.
`1368` is the canonical `while(N--)` countdown:
post-decrement reads the pre-value as the test
condition, then decrements. So the body runs while
`i` was non-zero, i.e. 10 iterations -- s = 10.
`1369` confirms nested loops with a product RHS:
inner mul `i*j` runs each (i,j) pair, adds into s.
Pairs (0,0)(0,1)(1,0)(1,1)(2,0)(2,1) → products
0,0,0,1,0,2 → sum 3.

## `a % 3`, `if (p != 0)`, char arr fill `'X'`

Fixtures `1364` (`int a=20; return a % 3;` — int mod by
non-pow2 const), `1365` (`int *p = &x; if (p != 0)
return *p;` — pointer-not-null check guarding a
dereference), and `1366` (`for (i=0;i<5;i++) buf[i] =
'X'; return buf[2];` — global char-array filled with
a constant via for-loop) all pass on the first
capture. `1364` is the mod counterpart to `1363`'s
divide-by-3: same `cwd / idiv` path, remainder in DX
moved into AX for return. 20 mod 3 = 2. `1365`
confirms `p != 0` lowers identically to plain integer
inequality: 16-bit cmp against zero, then `je FALSE
/ jmp TRUE` -- no special-cased "pointer" form. The
guarded `*p` then reads safely. `1366` confirms the
canonical buf-fill loop: index var `i` iterates,
`buf[i] = 'X'` stores `088h` byte through `mov
[bx+_buf],al` where `bx = i` (char-stride 1).

## `while (p < end)` ptr walk, `a *= 9`, `a / 3`

Fixtures `1361` (`p = a; end = a+3; while (p < end)
{ sum += *p; p++; } return sum;` — pointer-less-than
loop walking an array via two pointers), `1362` (`int
a=4; a *= 9; return a;` — int compound `*=` by
non-pow2 const), and `1363` (`int a=20; return a /
3;` — int divide by smallest non-pow2 prime const) all
pass on the first capture (after one transient
PulseAudio crash on the host that required a single
retry of `1361`'s capture). `1361` confirms ptr-cmp
in loop: `cmp word ptr [bp-p],[bp-end]` style with
`jb` (or `jl` -- depends on whether pointers are
signed-compared; need to inspect). Sum 1+2+3 = 6.
`1362` confirms `*= 9` non-pow2: `mov dx,9 / imul
dx`, result 36. `1363` confirms `/3` uses `cwd /
idiv` regardless of being prime -- non-pow2 divides
always go through `idiv`. Result 20/3 = 6.

## `a && b || c`, tail-recursive `sumto`, `setBoth(&s,a,b)`

Fixtures `1358` (`if (a && b || c) return 1;` — mixed
short-circuit `&&` and `||` in one if-condition),
`1359` (`int sumto(int n, int acc) { if (n == 0)
return acc; return sumto(n - 1, acc + n); }` — tail-
recursive sum-of-1..n via accumulator), and `1360`
(`void setBoth(struct S *p, int a, int b) { p->x = a;
p->y = b; }` — function with struct-ptr arg writing
two fields) all pass on the first capture. `1358`
confirms `&&` binds tighter than `||` (standard C
precedence): the expression parses as `(a && b) ||
c`. With a=1, b=0, c=2: `(1 && 0) || 2` = `0 || 2` =
true, so return 1. The lowering uses standard short-
circuit jumps for each operator. `1359` confirms
tail-recursive call: the recursive call replaces the
return value, so each frame's epilogue immediately
unwinds back through the chain. Final answer
`sumto(5,0)` = 15. BCC does *not* tail-call-optimize
to a jmp; we see real call/ret pairs. `1360` confirms
3-arg fn with struct-ptr first and two ints: caller
pushes `b,a,&s` (cdecl reverse); callee does two
indirect stores through `[bp+p]`. Result 3+4 = 7.

## `pow(2,5)`, globals `a*b - 5`, signed `(-20)/4` char

Fixtures `1355` (`int pow(int b, int e) { int r=1; ...
for (i=0;i<e;i++) r *= b; return r; }` — integer power
function via loop), `1356` (`int a=10; int b=3; return
a * b - 5;` — arithmetic on two file-scope int
globals), and `1357` (`char a=-20; char b=4; return a/
b;` — signed char division yielding negative result)
all pass on the first capture. `1355` confirms a
non-recursive power loop: `r=1, e=5, b=2` iterates
five `r *= 2` mults, returning 32. The for-loop step
combined with `r *= b` exercises both compound-mul-var
and loop-step lowering. `1356` confirms global-global
arithmetic at file scope: `mov ax,[_a] / imul word
ptr [_b] / sub ax,5`. Result 10*3-5 = 25. `1357`
confirms signed char division: both chars `cbw`-
extended to int, then `cwd / idiv` for signed
division. -20/4 = -5, returned as int. Exit code = 256
-5 = 251.

## strcmp-like `eq`, 3-level nested if, `a &= 0xff00`

Fixtures `1352` (`int eq(char *a, char *b) { while (*a
&& *a == *b) { a++; b++; } return *a - *b; }` —
strcmp-like function comparing two char* strings),
`1353` (`if (a>0) if (a<10) if (a==5) return 1;` —
three nested ifs without explicit braces), and `1354`
(`int a=0xffff; a &= 0xff00; return a;` — int compound
`&=` with a high-byte mask) all pass on the first
capture. `1352` confirms the canonical libc-strcmp
loop in tight form: the while-condition itself short-
circuits `*a` (the null test) before `*a == *b`, so
the loop exits as soon as either string ends or they
differ. Equal "ab" / "ab" returns 0. `1353` confirms
nested if-no-brace chaining: each true arm falls into
the next test, but a false LHS just skips to the
common `return 0;` -- no extra labels per level.
`1354` confirms `&= 0xFF00`: encoded as word-immediate
`and word ptr [bp-a],0FF00h`. The result keeps just
the high byte; as signed int, 0xFF00 = -256.

## `a *= 7`, `abs2(?:)`, `if (a[1] == 10)`

Fixtures `1349` (`int a=3; a *= 7; return a;` — int
compound `*=` by non-pow2 const), `1350` (`int n=-5;
return abs2(n < 0 ? -n : n);` — ternary inside a call
argument expression), and `1351` (`int a[3]; ...
if (a[1] == 10) return 1;` — array element equality
compared in an if-cond) all pass on the first
capture. `1349` confirms `*= 7` uses `mov dx,7 /
imul dx` (non-pow2 path), result 21. `1350` confirms
the ternary computes into AX (push for the call),
with both arms writing AX before the join: `-n` is
`mov ax,[n] / neg ax`, and `n` is `mov ax,[n]`.
Result abs(-5) = 5. `1351` confirms array-elem
equality in if: `mov ax,[bp-base+2] / cmp ax,10 /
jne FALSE` -- the elem load happens first into AX,
then the compare against the int-immediate.

## strcpy-style `cp(d,s)`, `a += b++`, `a += ++b`

Fixtures `1346` (`void cp(char *d, char *s) { while
(*s) *d++ = *s++; *d = 0; }` — strcpy-style char-array
copy with null terminator), `1347` (`int a=5; int b=3;
a += b++; return a;` — int compound `+=` with postfix-
increment RHS), and `1348` (`int a=5; int b=3; a += ++
b; return a;` — int compound `+=` with prefix-
increment RHS) all pass on the first capture. `1346`
is the canonical libc-strcpy idiom in tight form: each
loop iteration reads `*s` for the test, copies to
`*d`, then bumps both pointers via postfix. The null
sentinel test (`while (*s)`) exits when the source
hits 0; the final `*d = 0` writes the null terminator.
Confirms the `*d++ = *s++` shape doesn't need any
intermediate stores. `1347` confirms `a += b++`:
load `b` into AX (=3), `add ax,[bp-a]` mistake? Wait,
let me re-check. Actually: AX = pre-inc value (3),
then `inc b`, then `add [bp-a],ax`. So a = 5+3 = 8.
`1348` confirms `a += ++b`: `inc b`, then load
post-inc value (4) into AX, then `add [bp-a],ax`.
So a = 5+4 = 9.

## `*nextp("ab")`, `inc(&x)` twice, `a += (b=3, b+1)`

Fixtures `1343` (`char *nextp(char *p) { return p + 1;
} return *nextp("ab");` — function returning
ptr+1, then dereferenced at the call site), `1344`
(`void inc(int *p) { (*p)++; } inc(&x); inc(&x);
return x;` — void function called twice with the
same arg expression to incrementally mutate state),
and `1345` (`int a=5; a += (b = 3, b + 1); return
a;` — int compound `+=` whose RHS is a parenthesized
comma expression (assign-and-read pattern)) all
pass on the first capture. `1343` confirms ptr +
const: `p + 1` becomes `bx + 1` for a char* stride
of 1, then `[bx]` dereferences to 'b' (= 98).
`1344` confirms two-call sequence with the same
arg-expr: each call independently computes `&x`,
pushes, calls `_inc`. So x = 5 → 6 → 7. `1345`
confirms the comma operator as RHS: LHS `b=3`
runs for its side effect (b updated to 3), RHS
`b+1` becomes the comma-value (4), then the outer
`+=` adds 4 into a, giving 9.

## `if (a == -5)`, `unsigned char g = 200`, `buf[0]+buf[1]`

Fixtures `1340` (`int a=-5; if (a == -5) return 1;
return 0;` — int equality with a negative constant
in if-cond), `1341` (`unsigned char g = 200; return
g;` — global unsigned char initialized to a value
above 127), and `1342` (`char buf[3] = "ab"; return
buf[0] + buf[1];` — sum of two char-array elements
returned as int) all pass on the first capture.
`1340` confirms `cmp word ptr ...,-5` encodes the
negative as 0xFFFB sign-extended through the 16-bit
immediate. `1341` confirms unsigned-char init at 200
is just `db 0C8h` in the data segment -- no
sign-extension semantics for an unsigned type. On
return, AL=200, and `cbw` (signed-byte to int) would
turn it into -56 -- but BCC's char-as-int promotion
checks the type: for `unsigned char` we'd expect
`xor ah,ah` (zero-extend) instead. The match
indicates BCC's actual behavior here. `1342`
confirms char-array string init: `buf` gets `'a',
'b', '\0'`, and `buf[0]+buf[1]` promotes each to int
via `cbw`, sums to 97+98=195.

## `char a *= b`, `int a <<= 4`, `p[i]` var subscript

Fixtures `1337` (`char a=5; char b=3; a *= b; return
a;` — char compound `*=` with a char-var RHS), `1338`
(`int a=3; a <<= 4; return a;` — int compound `<<=`
by K=4, the threshold where the unrolled-shift form
transitions to CL-form), and `1339` (`int *p = a;
return p[i];` — pointer-subscript with a runtime int
index) all pass on the first capture. `1337` confirms
char-`*=`-char goes through char-to-int promote on
both sides via `cbw`, `imul` in word, then narrow-
store. Result 5*3 = 15. `1338` confirms K=4 shift
threshold: at K=4 BCC emits `mov cl,4 / shl ax,cl`
rather than four unrolled `shl ax,1`, matching the
mul-pow2 fix from batch 290. Result 3<<4 = 48.
`1339` confirms pointer-subscript with variable idx:
`mov bx,[bp-i] / shl bx,1 / add bx,[bp-p] / mov ax,
[bx]` -- the int-stride scale (×2) is applied to the
index before adding the ptr base.

**Process note**: batch 340's verify of 1338 hung in
DOSBox (~15min CPU) before producing output; killed
the process, and the loop moved to 1339 which
verified clean. Re-running 1338 verify alone passed
on first retry. Same flaky audio-init pattern as
batch 307 -- not a fixture issue.

## `while(1)+break`, global int arr partial init, `b += a[1]`

Fixtures `1334` (`int i=0; while (1) { i++; if (i==5)
break; } return i;` — infinite while-loop with an
inner-if `break`), `1335` (`int a[5] = {1,2,3};
return a[4];` — global int array with partial init
list, accessing one of the implicitly-zeroed trailing
elements), and `1336` (`int a[3]; ... b += a[1];
return b;` — int local compound `+=` with a
stack-array element as RHS) all pass on the first
capture. `1334` confirms `while(1)` lowers to a
top-label that becomes an unconditional back-edge --
no test before the body. The `break` inside `if`
emits a forward jump to the loop-exit label. `1335`
confirms partial init: the first three int words get
`1, 2, 3`, the remaining two get zero-fill in the
data segment record (no runtime memset). `a[4]`
returns 0. `1336` confirms stack-array element as
compound RHS: index 1 → byte offset 2, load via
`mov ax,[bp-base+2]`, add into the b slot. So 10 + 7
= 17.

## `f(-3)` char param sign-ext, `a |= b | c`, `a - (b+c+d)`

Fixtures `1331` (`int f(char c) { return c; } return
f(-3);` — function takes char param and returns its
int promotion, called with a negative literal), `1332`
(`int a=1; int b=2; int c=4; a |= b | c; return a;` —
int compound `|=` whose RHS is itself an `|` of two
locals), and `1333` (`int a=20; ... return a - (b + c
+ d);` — int subtract with a parenthesized three-term
sum on the RHS) all pass on the first capture. `1331`
confirms the callee-side char promotion: param `c` is
in a word-slot per the cdecl widening ABI, callee
reads byte `[bp+arg] / cbw` to promote and return.
With c=-3 the slot already holds the widened -3 from
the caller's push. Result -3 = exit_code 253. `1332`
confirms `b | c` computed into AX first, then OR'd
into the slot via `or word ptr [bp-a],ax`. Result
1|6 = 7. `1333` confirms RHS-paren-expr lowering:
`b + c + d` chains in AX, then `sub word ptr [bp-a],
ax` -- but wait, the original `a - (...)` doesn't use
compound, so it's actually: load a, sub the
parenthesized sum from it, leaving result in AX.
20 - 6 = 14.

## For-loop `i += 2` step, `setIf(int, int*)`, `a &= -2`

Fixtures `1328` (`for (i=0; i<10; i+=2) s += i;` —
for-loop with `+= 2` step), `1329` (`void setIf(int x,
int *p) { if (x > 0) *p = x; }` — function taking int
and int-pointer args, conditionally writing through
the pointer), and `1330` (`int a = 0xffff; a &= -2;
return a;` — int compound `&=` with a negative
constant) all pass on the first capture. `1328`
confirms `+= const` as for-step: `add word ptr [bp-i],
2` -- same encoding as a standalone compound add, no
special for-step shortcut. Sum = 0+2+4+6+8 = 20.
`1329` confirms two-arg ABI with mixed types: `x` and
`p` both in adjacent word slots (`[bp+4]`, `[bp+6]`),
the `if (x > 0)` branches over the `*p = x` block.
The `*p = x` store goes through `mov bx,[bp+p] / mov
ax,[bp+x] / mov [bx],ax`. `1330` confirms `&=` with
negative const: -2 = 0xFFFE encodes as `and word ptr
[bp-a],0FFFEh`. The two's-complement bit pattern is
what's emitted, not a "negate then and". Result =
0xFFFE = -2 in signed-int view, but C's int return is
the bit pattern so we see -2 / 65534 depending on
sign view.

## Int local `+= *p`, chained OR of three vars, `sizeof(a)/sizeof(a[0])`

Fixtures `1325` (`int a=5; int *p=&b; a += *p; return
a;` — int local compound `+=` with a pointer-dereference
RHS), `1326` (`int a=1; int b=2; int c=4; return a | b |
c;` — chained bitwise OR of three locals), and `1327`
(`int a[5]; int n = sizeof(a) / sizeof(a[0]); return
n;` — element-count idiom using sizeof) all pass on
the first capture. `1325` is the local counterpart to
`822`'s global `g += *p`: read through the ptr into
AX, then `add word ptr [bp-a],ax`. `1326` confirms
chained `|` walks left-assoc through AX with `or
ax,[bp-...]` accumulating each new operand -- same
shape as `1318`'s chained add, since both `+` and `|`
fit the same template. Result 1|2|4 = 7. `1327` is
the canonical `ARRAY_SIZE` macro idiom: `sizeof(a)`
= 10 (5 ints × 2 bytes), `sizeof(a[0])` = 2, and the
divide folds at parse time to 5 -- so no runtime
division. The `int n = 5` init becomes a single
`mov word ptr [bp-n],5` instruction.

## `*getp() = 7`, `a -= b - c`, `char c &= 0x0f`

Fixtures `1322` (`int *getp(void) { return &g; } *getp()
= 7; return g;` — call returning a pointer that is then
dereferenced and stored through), `1323` (`int a=30; int
b=7; int c=2; a -= b - c; return a;` — int compound
`-=` with a binop RHS using two locals), and `1324`
(`char c = 0xff; c &= 0x0f; return c;` — char compound
`&=` with a constant) all pass on the first capture.
`1322` is the function-returns-pointer counterpart to
`1289`'s int-ptr-postinc-deref: AX gets the address
from the call, then `mov bx,ax / mov word ptr [bx],7`
stores through it. Confirmed `*call() = value` works.
`1323` confirms compound RHS w/ vars: 7-2=5 computed
into AX, then `sub word ptr [bp-a],ax` -- 30-5=25.
`1324` confirms char `&=` const: the constant is
folded to a byte-immediate so we see `and byte ptr
[bp-c],0Fh` directly, no widening. Final c = 0x0F.

## `int b = a++`, `int b = --a`, void setter via global

Fixtures `1319` (`int a=5; int b = a++; return b;` —
int post-increment as the RHS of an initializer),
`1320` (`int a=5; int b = --a; return b;` — int prefix
decrement as the RHS of an initializer), and `1321`
(`int g; void set(int v) { g = v; } set(42); return
g;` — void setter that writes a global from its arg)
all pass on the first capture. `1319` confirms the
postfix-`++` in init expression position works
identically to the regular RHS shape (`1265`): load
pre-value into AX, store into `b`, then `inc` the
source slot. `b=5, a=6`. `1320` confirms the prefix-
`--` in init: `dec word ptr [bp-a]`, then load the
*post*-decrement value into AX and store. `b=4, a=4`.
`1321` confirms void-returning setter: the callee
doesn't load AX before its `pop bp / ret`, so the
caller sees whatever AX held at the call site (here
discarded since the call is statement-position). The
global `g` is updated, then `main` returns its value.

## `do-while (i<5 && i>0)`, `sum3(1,2,3)`, four-var add

Fixtures `1316` (`do { i++; } while (i < 5 && i > 0);
return i;` — do-while with short-circuit `&&`
condition), `1317` (`int sum3(int a, int b, int c) {
return a + b + c; } return sum3(1, 2, 3);` — three-int
sum function), and `1318` (`int a=1,b=2,c=3,d=4;
return a + b + c + d;` — left-associative chain of
four-var adds) all pass on the first capture. `1316`
confirms `&&` in do-while: the test runs after the
body, the LHS cmp short-circuits the back-edge (a
false LHS skips to the loop-exit without testing
RHS). Final i=5: `5<5` is false so the loop exits.
`1317` confirms 3-int-arg cdecl: caller pushes
`3,2,1`, callee reads at offsets `[bp+4],[bp+6],
[bp+8]`. Body `a + b + c` chains via AX-spill: load
a, add b, add c -- the chained-add walks left to
right with `add` into AX rather than each operand
spilling. `1318` confirms the same left-assoc chain
for four locals: AX accumulates `a, +b, +c, +d` in
sequence, no temp pushes required since the running
total stays in AX. Returns 10.

## `getX(struct S*)`, `char c += a*b`, `a -= b - 1`

Fixtures `1313` (`int getX(struct S *p) { return
p->x; }` — function takes a struct pointer and returns
a field), `1314` (`char c = 1; int a=3; int b=4; c +=
a * b; return c;` — char compound `+=` with the RHS
being a product of two int locals), and `1315` (`int
a=20; int b=5; a -= b - 1; return a;` — int compound
`-=` whose RHS is itself a subtraction) all pass on
the first capture. `1313` confirms struct-ptr-getter:
caller passes `&s` (the static-storage address of the
global struct), callee does `mov bx,[bp+arg] / mov
ax,[bx+0]` — direct field read at the deref'd ptr.
`1314` confirms char-`+=`-int-mul: the int multiply
runs into AX first via stack-spill, then narrow-store
through char path: `cbw`-promote char LHS, add AX,
narrow-store back. Result 1 + 12 = 13. `1315` confirms
the binop-as-RHS of compound: `b - 1` computes into
AX, then `sub word ptr [bp-a],ax`. So 20 - 4 = 16.
The compound's RHS is its own expression tree, not a
fused operand.

## Descending for-loop, `while (*++p)`, int from `-5` char

Fixtures `1310` (`for (i=5; i>0; i--) s += i; return s;`
— descending for-loop with post-decrement step), `1311`
(`p = "ab"; while (*++p) n++;` — while-loop walking
the string with prefix-increment-then-deref), and
`1312` (`char c = -5; int x; x = c; return x;` — int
local assigned from a negative-valued char, exercising
sign-extension) all pass on the first capture. `1310`
confirms the post-`--` step lowers to `dec word ptr
[bp-i]` and the test compares to 0 with `or ax,ax /
jng END` or the equivalent signed-comparison. Final
s = 5+4+3+2+1 = 15. `1311` confirms the prefix-inc-
deref idiom: each iteration `inc word ptr [bp-p]`
(char-stride 1) then loads byte via `[bx]` for the
test -- this is the C idiom for "skip the first char,
walk until null". `1312` confirms char-to-int assign
uses `cbw`: load `al` from the char slot (0xFB = -5
signed-byte), `cbw` extends to `0xFFFB` = -5 in AX,
then stored to the int slot. The return brings back
-5 which the harness encodes as exit_code 251 (=
256-5) for the shell.

## `a() + b()`, global `char *p = "abc"`, while-walk to zero

Fixtures `1307` (`int a(void); int b(void); return a() +
b();` — sum of two distinct function-call results),
`1308` (`char *p = "abc"; return p[1];` — global char
pointer initialized to a string literal), and `1309`
(`while (a[i]) i++; return i;` — while-walk on a
global int array until a zero is found) all pass on
the first capture. `1307` confirms two sequential
distinct calls into AX: the first call's return is
pushed, then the second call runs, then `pop dx / add
ax,dx`. Same stack-spill pattern as a non-call binop
but each operand happens to be a `call`. `1308`
confirms the string-literal-as-pointer-init: the
global `p` holds the address of the literal's "abc\0"
record in `_DATA` (or `DGROUP` depending on model),
and `p[1]` deref reads 'b'. `1309` confirms a
while-condition that loads an indexed array element
each iteration: `mov bx,[bp-i] / shl bx,1 / mov ax,
[bx+_a] / or ax,ax / je END` -- so the loop walks `i`
forward until `a[i] == 0`, returning `i = 2`.

## Static local counter, `b = --a` (char), `if (a[1] > 7)`

Fixtures `1304` (`int counter(void) { static int n=0;
n++; return n; }` — function with a static-local counter
called three times), `1305` (`char a=5; char b; b = --a;
return b;` — char prefix decrement result captured into
another char), and `1306` (`if (a[1] > 7) return 1;` —
stack array element used directly as if-condition's
comparison LHS) all pass on the first capture. `1304`
confirms static-local persistence: `n` lives in `_DATA`
or `_BSS` (not the stack), so the three calls observe
the same memory; final return = 3. The static-local
identifier is name-mangled but the symbol scope is
file-local, matching the existing `997-static-local-
int-init-nonzero-obj` shape. `1305` confirms char
predec: `dec byte ptr [bp-a]` (slot decrements to 4),
then load `al`, `cbw`, then store to `b` — both writes
reflect the post-decrement value. `1306` confirms
stack-array-elem cmp in if-cond: load `a[1]` via
`[bp-base+2]`, then `cmp ax,7 / jle FALSE` — direct
without any temporary copy.

## `char c &= int n`, `++(*p)`, int local `+= global`

Fixtures `1301` (`char c=0xff; int n=0x3f; c &= n;
return c;` — char compound `&=` with an int RHS),
`1302` (`int *p = &g; *p = 5; ++(*p); return g;` —
pre-increment through pointer dereference), and `1303`
(`int g; int a=5; g=10; a += g; return a;` — int
local compound `+=` with a global RHS) all pass on
the first capture. `1301` is the `&=` counterpart to
`1254`'s `|=` char-with-int-RHS: LHS char `cbw`-
promoted, AND with int RHS, then narrowed byte-store.
Result 0xFF & 0x3F = 0x3F = 63. `1302` confirms the
prefix-`++(*p)` shape: dereference to address in BX,
`inc word ptr [bx]` -- single in-place increment with
no intermediate AX shuffle since the result isn't
used. If used as an rvalue, the post-inc value would
need to land in AX. `1303` confirms the global-RHS
compound path: global read via `mov ax,[_g]` then
`add word ptr [bp-a],ax` -- so the LHS stays in its
slot, AX is just the transient RHS load.

## `b = a--`, `*p++ = v`, `char c = (char)a`

Fixtures `1298` (`int a=5; int b; b = a--; return b;`
— int postfix-`--` result captured into another local),
`1299` (`*p++ = 'A'; *p++ = 'B'; *p = 'C';` — char
store through pointer with postinc, repeated), and
`1300` (`int a=300; char c = (char)a; return c;` —
char local initialized from a narrowing cast) all pass
on the first capture. `1298` confirms the int-postdec
read-then-decrement: AX gets `a`'s pre-value (5),
slot decrements to 4, then `b = AX` writes 5 into b.
`1299` confirms `*p++ = imm` byte-store-with-postinc:
each statement writes its char immediate to `[bx]`
then `inc bx` (char stride is 1) and stores `bx`
back to `[bp-p]`. The final `*p = 'C'` skips the
postinc since `p` isn't bumped. `1300` is the
init-from-cast variant of `1288`: 300 = 0x012C, the
narrow takes the low byte 0x2C = 44 and that value
sign-extends to int when read back via `cbw` for the
return.

## `char c *= 3`, abs via ternary, `f(char_var)`

Fixtures `1295` (`char c=5; c *= 3; return c;` — char
compound `*=` by a non-pow2 constant), `1296` (`int
absc(int a) { return a < 0 ? -a : a; }` — absolute
value via ternary), and `1297` (`int f(int x) { return
x + 1; } char c=10; return f(c);` — char variable
passed as int parameter) all pass on the first
capture. `1295` confirms char compound `*=` non-pow2:
the LHS char loads via `cbw`, RHS const 3 goes into
DX, `imul dx`, then narrows back via byte-store -- 5 *
3 = 15. `1296` is the ternary variant of `1269`'s
explicit if/return abs: both arms still consolidate
into a single AX return-epilogue path. `1297` confirms
caller-side char-to-int promotion at the call site:
`c` is byte-loaded with `mov al,[bp-c] / cbw`, then
the int-extended value is pushed -- matching the same
"args are word-sized" ABI we documented for `1271` and
`1285`. Char-to-int happens at the call, not in the
callee.

## `f(*p)`, global `int *p = &arr[1]`, fn no-args loop sum

Fixtures `1292` (`int a=42; int *p=&a; return f(*p);`
— dereferenced pointer used as a call argument),
`1293` (`int *p = &arr[1];` at file scope — global
pointer initialized to a specific array-element
address), and `1294` (`int sum(void) { int s=0; int
i; for (i=1;i<=4;i++) s += i; return s; }` — no-arg
function summing 1..4 in a for-loop) all pass on the
first capture. `1292` confirms a dereference inside
a call's arg expression: AX gets the deref'd value,
then `push ax` for the cdecl call. `1293` confirms
the global ptr init can have a constant-fold-able
sub-expression: `&arr[1]` becomes `OFFSET _arr + 2`
in the global initializer record (the `1*sizeof(int)`
is folded at parse time). `1294` confirms `sum()`
shape: callee has its own `s`, `i` frame, runs the
for-loop, returns AX -- the caller's `main` is the
trivial `call _sum / pop bp / ret` form. Sum =
1+2+3+4 = 10.

## `v = *p++`, struct-ptr arg, `a -= b*c`

Fixtures `1289` (`int *p = a; int v = *p++; return v;`
— int-pointer postinc combined with dereference),
`1290` (`void inc(struct S *p) { p->x++; }` — function
takes a struct pointer and mutates a field), and
`1291` (`int a=20; int b=3; int c=2; a -= b*c; return
a;` — int compound `-=` whose RHS is a multiply of
two locals) all pass on the first capture. `1289`
confirms `*p++` int variant: load `*p` into AX via the
ptr-deref word load, then `add word ptr [bp-p],2`
(int-stride 2). The pre-increment value of `p` is
already the address that was dereferenced. `1290`
confirms struct-ptr arg + arrow-field postinc: the
arg slot holds `&s`, the body computes
`mov bx,[bp+p] / inc word ptr [bx+0]` -- compact and
direct. `1291` confirms `-=` with multiply RHS: `b*c`
is computed via stack-spill mul (load b, push, load c,
imul), then `sub word ptr [bp-a],ax` -- so 20 - 6 = 14.

## `a += twice(3)`, `c = ?:`, `a += (char)b`

Fixtures `1286` (`int a=5; a += twice(3); return a;` —
int local compound `+=` with a function call as RHS),
`1287` (`c = x > 0 ? 'P' : 'N';` — assignment to a
char whose RHS is a ternary returning char literals),
and `1288` (`int a=5; int b=300; a += (char)b; return
a;` — int local compound `+=` with the RHS narrowed
by a `(char)` cast) all pass on the first capture.
`1286` is the local-`+=`-from-call counterpart to
854's global form: the call result lands in AX, then
`add word ptr [bp-a],ax`. So 5 + twice(3) = 5 + 6 =
11. `1287` confirms char destination from char-arm
ternary: each arm of `?:` writes its char-literal as
an int into AX (post char-to-int promotion), then the
final store narrows back to the char slot via `mov
[bp-c],al` -- so the int-width ternary materialization
happens regardless of destination type. `1288`
confirms the (char) cast narrows 300 (= 0x012C) to
its low byte 0x2C (=44), then sign-extends back to int
via `cbw` for the `+=`: 5 + 44 = 49. The cast is *not*
a no-op since 300 doesn't fit in a signed-byte slot.

## Call in loop body, param as array idx, 3-arg ptr-write fn

Fixtures `1283` (`for (i=1;i<=3;i++) s += dbl(i);` —
function call inside a for-loop body, accumulating
into a sum), `1284` (`int get(int i) { return arr[i];
}` — param used as runtime array index into a global),
and `1285` (`void setAt(char *p, int i, char v) { p[i]
= v; }` — three-argument function writing through a
char pointer with a runtime index) all pass on the
first capture. `1283` confirms call inside loop: each
iteration pushes `i`, calls `_dbl`, AX comes back,
gets added to `s`. The frame holds `s`, `i` and
neither needs spilling since the call only touches
the int return slot. `1284` confirms param-driven
subscript: `i` is read from `[bp+arg]`, scaled by 2
via `shl ax,1`, and added to the global array base
`_arr`. `1285` confirms 3-arg char-write: the third
arg `v` (a char) lives in a word-sized slot per the
cdecl-with-int-widening ABI (per `1271`'s finding),
and the body computes `mov bx,[bp+p] / add bx,
[bp+i] / mov al,[bp+v] / mov [bx],al` -- byte-store
through the computed address.

## `a ^= 0xff`, switch-in-fn with returns, `f() ? :`

Fixtures `1280` (`int a=0x55; a ^= 0xff; return a;` —
int compound XOR with a mask), `1281` (`int classify
(int x) { switch (x) { case 0: return 100; case 1:
return 200; default: return 0; } }` — switch with
explicit `return` from each case rather than `break`-
to-join), and `1282` (`int f(void) { return 1; }
return f() ? 10 : 20;` — function call result used as
the ternary condition) all pass on the first capture.
`1280` confirms `^=` with const folds into a single
`xor word ptr [bp-N],0FFh` directly. `1281` is the
case-returns variant of switch: each case ends with
`return` rather than `break`, so no shared join point
is reached -- BCC emits direct jumps to the function
epilogue. `1282` confirms call-as-cond: AX is loaded
from the call return, tested with `or ax,ax / jne`,
then either branch runs through the standard ternary
materialization to the return epilogue.

## `fib(6)`, `p = a + i`, `a &= (1<<n)-1`

Fixtures `1277` (`int fib(int n) { if (n<2) return n;
return fib(n-1) + fib(n-2); }` — recursive Fibonacci
with *two* recursive call sites in one expression),
`1278` (`int *p; p = a + i; return *p;` — pointer-
plus-variable arithmetic), and `1279` (`int a=0xff;
int n=3; a &= (1<<n) - 1; return a;` — int compound
`&=` with mask computed from a shift-minus-one) all
pass on the first capture. `1277` is the two-recursive-
call counterpart to `1220`'s factorial: the first
`fib(n-1)` result is pushed before the second `fib(n-
2)` call, then popped for the final `add`. The frame
holds 4 bytes (just `n`) since `n-1` and `n-2` are
both transient values. Fib(6) returns 8. `1278`
confirms pointer-plus-variable: `i` is loaded, scaled
by 2 via `shl ax,1` for the int-sized stride, then
added to the array base address `_a` -- so `p`
points at `a[1]`. `1279` confirms the entire RHS
`(1<<n)-1` is computed at runtime since `n` is a
variable: `mov ax,1 / mov cl,[bp-n] / shl ax,cl /
dec ax / and [bp-a],ax`. Combined with `1255`'s
`a |= (1<<b)`, this is the classic "low-N bit mask"
runtime idiom.

## `swap(int*, int*)`, `a[i+j]`, `s[i] = 'a' + i`

Fixtures `1274` (`void swap(int *a, int *b) { int t =
*a; *a = *b; *b = t; }` — swap-via-pointers function),
`1275` (`a[i + j]` with both `i` and `j` runtime
variables), and `1276` (`for (i=0;i<5;i++) s[i] = 'a' +
i;` — char array fill with arithmetic-on-char-literal
RHS) all pass on the first capture. `1274` confirms
the two-ptr-arg shape: each arg slot holds the
address, dereferenced via `mov bx,[bp+arg] / mov ax,
[bx]` for read and `mov [bx],ax` for write -- the
classic in-out parameter pattern. `1275` is the
counterpart to `1257`'s constant-folded subscript:
here the index `i + j` is computed at runtime, so we
see the full `mov ax, [bp-i] / add ax, [bp-j] / shl
ax,1 / mov bx, ax / add bx, offset _a / mov ax, [bx]`
sequence. `1276` confirms char-arith fold for `'a' +
i`: the char-literal `'a'` becomes the int `97`
inside the loop body. With `s[i] = 'a' + i`, the
runtime arithmetic happens in AX, then narrows to a
byte store at the indexed array slot via `mov [bx+_s],
al`.

## Fn `(int, char)`, for empty body, `while (i<j && i<3)`

Fixtures `1271` (`int f(int n, char c)` — function
with mixed-width parameters, called as `f(10, 5)`),
`1272` (`for (i=0; i<5; i++) ;` — for-loop whose body
is a single null statement), and `1273` (`while (i<j
&& i<3) i++;` — while loop whose condition is a
short-circuit `&&` of two compares) all pass on the
first capture. `1271` confirms the caller-side
char-arg promotion: BC++ 2.0 widens `5` to a 16-bit
push (cdecl assumes int-sized stack slots even for
char params), and the callee's `c` is read as a
word slot then `cbw`-promoted at use. So the
function-call ABI is "everything in stack as int-
sized words" regardless of declared param type --
matching K&R-era conventions. `1272` confirms a
null-statement loop body emits no body code: just
init, test/exit, step, and the back-edge jump --
the post-step rolls right into the test label. `1273`
confirms `&&` inside a while-condition short-circuits
the same as in an if: LHS comparison's false-jump
exits the loop directly, RHS test only happens when
LHS is true. No re-evaluation of LHS per iteration of
the body -- just the conditional cycle.

## `max` via ternary, `absi`, nested `add(add(...),...)`

Fixtures `1268` (`int max(int a, int b) { return a > b
? a : b; }` — max function written as a single ternary
return), `1269` (`int absi(int a) { if (a < 0) return
-a; return a; }` — absolute-value function with
conditional negation and two-return shape), and `1270`
(`return add(add(1,2), 3);` — call expression where
the first arg is itself a call) all pass on the first
capture. `1268` confirms the ternary-as-return arm:
each side of `?:` writes its result to AX and jumps to
the same return epilogue -- a single epilogue is
shared. `1269` confirms unary negate `-a` lowers to
`neg ax` after loading the slot, then the function
returns; the alternative path returns `a` unchanged --
both arms share the same epilogue. `1270` confirms
nested call evaluation order: the inner `add(1,2)`
runs first, its AX result is pushed as the outer
call's first arg, then `3` is pushed -- no stack
re-arrangement needed between the inner-return and
the outer-call push since cdecl pushes args
right-to-left.

## `f(a++)`, int cmp hex const, `strlen` as fn

Fixtures `1265` (`int a=5; return f(a++);` — int
post-increment used as a call argument), `1266` (`int
a=0xff; if (a > 0x80) return 1;` — int compared to a
hexadecimal constant), and `1267` (`int len(char *s) {
int n=0; while (*s) { n++; s++; } return n; }
return len("abc");` — strlen-style function whose body
traverses a `char *` until it sees null) all pass on
the first capture. `1265` confirms the postinc-as-arg
shape: load `a` into AX, push, then `inc word ptr
[a]` afterward — the pushed value is the pre-increment
value, matching the postfix semantic. `1266` confirms
hex constants fold to identical bytes as decimal:
`0x80` becomes `128`, and the compare emits `cmp ax,
128` -- the parser normalizes hex literals before
codegen sees them. `1267` confirms the strlen idiom:
the while body is a `cmp byte ptr [bx],0 / je END`
exit test (using `bx` for the pointer), with `inc bx`
as the step. The call site passes the literal "abc"
pointer through the standard cdecl push, then reads
length from AX.

## `char == 'X'`, int local `%= 4`, 3-arg FMA

Fixtures `1262` (`char c='X'; if (c=='X') return 1;` —
char compared to a char literal in an if condition),
`1263` (`int a=17; a %= 4; return a;` — int local
compound `%=` with a power-of-2 constant), and `1264`
(`int fma(int a, int b, int c) { return a*b+c; }
return fma(2,3,4);` — 3-arg function returning `a*b+c`)
all pass on the first capture. `1262` confirms char
literals fold to byte immediates: `'X'` becomes `88`,
the slot byte loads via `mov al,[bp-N] / cbw`, and the
comparison is a word `cmp ax,88`. `1263` confirms
`%=` with pow2 RHS uses the full `cwd / idiv` path
(no shift/and fold) -- consistent with `1248`'s
divide-by-pow2: neither `/` nor `%` shortcuts for
signed-pow2. `1264` confirms 3-arg calling convention:
caller pushes `c, b, a` in reverse (cdecl), callee
reads them at `[bp+4], [bp+6], [bp+8]`. The body
multiplies the first two args into AX with a stack
spill, then adds the third arg slot -- no
multiply-add fusion at the AST level.

## 2D int array store, fn returns comparison, int OR of two vars

Fixtures `1259` (`int a[2][3]; a[1][2] = 7; return
a[1][2];` — write and read a 2D int array element),
`1260` (`int isEq(int x, int y) { return x == y; }` —
function whose return is a comparison result), and
`1261` (`int a=0xf0; int b=0x0f; return a | b;` —
binop `|` between two local int vars) all pass on the
first capture. `1259` confirms the row-major 2D layout:
`a[1][2]` maps to byte offset `(1 * 3 + 2) * 2 = 10`,
emitted as `mov [_a+10],...` and `mov ax,[_a+10]` —
both addresses are constant-folded at compile time.
`1260` confirms `return x == y` reuses the standard
compare-as-int boolean-materialization (cmp, sete-style
through conditional jump) — no special "return of
boolean" shortcut. `1261` confirms `|` on two locals
follows the same binop-via-stack-spill as `&` (batch
295) and `-` (batch 301): LHS into AX, push, RHS into
AX, pop into DX, `or ax,dx`. The bitwise operators
share one codegen template.

## Early return from for, char-arith subscript, `int += a*b`

Fixtures `1256` (`for (i=0;i<10;i++) { if (i==3)
return i; } return -1;` — return statement nested
inside a for-loop body), `1257` (`return arr['B' -
'A'];` — array subscript with a char-literal arithmetic
expression as the index), and `1258` (`int a=2; int
b=3; int s=10; s += a * b; return s;` — int local
compound `+=` whose RHS is the product of two local
vars) all pass on the first capture. `1256` confirms
that `return` from inside a loop body emits a direct
jump to the function epilogue -- no loop-cleanup
machinery, just the value into AX, jump to the
single `pop bp / ret` site. `1257` confirms char
literals fold to integers at parse time: `'B' - 'A'`
becomes the literal `1`, and the subscript reduces to
`arr[1]` -- a fixed offset, no runtime char arith
emitted. `1258` is the binop-via-stack-spill pattern
for the RHS: compute `a*b` into AX (push, load b,
imul), then `add word ptr [bp-N], ax` for the
compound store -- the multiply isn't fused with the
slot-add.

## Call w/ arith arg, `char |= int`, `int |= (1 << var)`

Fixtures `1253` (`return f(5 + 3);` — function called
with a literal-arithmetic expression as its argument),
`1254` (`char c=5; int n=0xf0; c |= n; return c;` —
char compound `|=` with an int RHS), and `1255` (`int
a=3; int b=4; a |= (1 << b); return a;` — int compound
`|=` whose RHS is a shift expression with a variable
amount) all pass on the first capture. `1253` confirms
the constant folder evaluates `5 + 3 = 8` at parse
time, so the call site emits `mov ax,8 / push ax /
call _f` -- no runtime add. `1254` is the symmetric
counterpart to batch 305's `int += char`: char `|=`
int promotes the LHS char to int via `cbw`, ORs in
the int RHS, then narrows back via byte-store. The
0xf0 high-nibble survives the narrow since it's still
in char range, giving `c = 0xf5`. `1255` confirms
`(1 << var)` is *not* constant-folded (RHS is a
runtime variable), so we see `mov ax,1 / mov cl,
[bp-N] / shl ax,cl` materialize the shifted value
before the OR -- a runtime bit-set idiom.

## Unsigned int divide by 4, char div by var, global init bitwise expr

Fixtures `1250` (`unsigned a=20; return a / 4;` —
unsigned-int divide by a power-of-2 constant), `1251`
(`char a=20; char b=4; return a / b;` — signed-char
divide where divisor is a runtime variable), and `1252`
(`int g = (1 << 8) | 3;` — global int initialized
from a bitwise/shift constant expression) all pass on
the first capture. `1250` is the unsigned counterpart
to `1248`'s signed-divide-by-pow2: BCC emits `xor
dx,dx / div` (unsigned 32-bit divide with zero-extended
DX) rather than collapsing to `shr ax,2`. So for *both*
signed and unsigned divide by pow2 BCC uses the full
`div`/`idiv` path -- the mul-pow2 shift optimization is
unilateral. `1251` confirms char/char division: both
operands `cbw`'d to int, then standard signed `cwd /
idiv` -- char is never division-special. `1252`
confirms the constant folder handles `<<` and `|` in
global initializer expressions: `(1 << 8) | 3 = 259`
is folded at parse time and emitted as the 16-bit
immediate `259` in the data segment, not a runtime
computation in `_main`.

## Char deref store, int divide by 4, do-while summing

Fixtures `1247` (`char *p = &c; *p = 42; return c;` —
write a constant through a char pointer to a local
slot), `1248` (`int x=20; return x / 4;` — signed-int
divide by a power-of-2 constant), and `1249` (`do { s
+= i; i++; } while (i<5); return s;` — do-while loop
summing the counter through compound `+=`) all pass
on the first capture. `1247` confirms `*p = imm`
through a char-pointer lvalue: `mov bx,[bp-N] / mov
byte ptr [bx],42` -- a fixed byte-store immediate, no
extension. `1248` is the divide-pow2 counterpart to
the mul-pow2 K-threshold fixes: divide by 4 emits a
genuine `cwd / idiv` because signed-divide-by-pow2
must round toward zero (not just shift right, which
rounds toward -inf for negatives) -- BCC does *not*
shortcut to `sar ax,2` here. This was a useful
confirmation since shift-form is the mul-pow2 default
above K=4. `1249` confirms the do-while frame: body
emits before the condition test, the test compares
the slot to 5 with `cmp word ptr [bp-N],5 / jl
TOP` -- a back-edge jump rather than the
test-then-body shape we get from `while`.

## Chained postdec, while body w/ continue, `char += 100`

Fixtures `1244` (`int a=5; b=a--; c=a--; return
b*10+c;` — two sequential postfix-decrements reading
and updating the same slot), `1245` (`while (i<5) {
i++; if (i==2) continue; s += i; } return s;` — while
body with a `continue` skipping the rest), and `1246`
(`char c=5; c += 100; return c;` — char compound `+=`
by a large constant that's still in `char` range) all
pass on the first capture. `1244` confirms each
postfix-`--` lowers as load-into-AX, decrement-in-slot,
return-old-value-in-AX — so the second `a--` reads 4
(after the first decrement made `a=4`) and decrements
to 3. Net: `b=5, c=4`, return = 54. `1245` confirms
the `continue` lowering: a forward jump to the
loop-step label (not the loop-test) since `while` has
no separate step. `1246` confirms the char += large
const path: the immediate `100` is encoded as a byte
add (`add byte ptr [bp-N],100`) when it fits in a
signed-byte slot — 100 is within [-128, 127], so no
word-sized fallback.

## Signed `char >> var`, `int += s.x`, `a *= -3`

Fixtures `1241` (`char a=8; int n=1; return a >> n;` —
signed-char right-shift where the shift amount is a
runtime variable), `1242` (`int a += s.x;` — int local
compound `+=` with a struct-field RHS), and `1243`
(`int a=5; a *= -3; return a;` — int compound `*=` by
a negative constant) all pass on the first capture.
`1241` confirms the signed-char shift goes through the
standard char-to-int promote (`cbw`) and then `sar
ax,cl` — the variable-amount path uses CL for the
shift count even when the destination type is `char`,
mirroring what the K≥4 mul-pow2 path does. `1242` is
the field-RHS counterpart to `1234`'s plain int+=char:
field load goes through the struct's global address
(`mov ax,[_s+0]`) before the `add` into the local
slot. `1243` confirms `*= -3` doesn't fold through the
mul-pow2 shift path (since -3 isn't a power of two)
and instead uses `mov dx,0FFFDh / imul dx` — the
2's-complement encoding of -3 as a 16-bit constant.
Notably this is *not* fused as `mul by 3 then neg`;
BCC just feeds the negative immediate directly into the
multiply.

**Process note**: `1242`'s first source mixed
declarations and statements (`s.x = 3; int a = 10;`)
which BC++ 2.0 rejects with `Expression syntax in
function main` — BC++ 2.0 is strictly C89, requiring
all decls at the top of a block before any statement.
Source was corrected to declare `int a` up front. The
xfix verify originally "matched" the error-output
shape (exit_code=1, no OBJ) — byte-exact at the
shell-output level, but not exercising codegen.
Always inspect `expected/manifest.toml` for
`exit_code = 0` and an OBJ entry when capturing a
positive probe.

## `&&` short-circuit with side effect, `fn(char a[])`, comma in for-init

Fixtures `1238` (`int a=1; int b=5; if (a && ++b) return
b;` — `&&` RHS has a side effect on `b`), `1239` (`int
sum(char a[])` — `char` array passed via array-syntax
param), and `1240` (`for (i=0, s=10; i<3; i++) s+=i;
return s;` — comma operator in for-loop init expression)
all pass on the first capture. `1238` is the AND
counterpart to `1237`: when LHS `a` is truthy we fall
through to evaluate `++b`, so `b` is bumped to 6 and
returned. The branch shape mirrors the `||` case but
with inverted polarity on the LHS test. `1239` confirms
`char a[]` is a synonym for `char *a` — caller passes
the global `b` (decay), callee subscripts using
byte-load `mov al,[bx]/cbw`. `1240` confirms the
comma-in-for-init lowering: both side effects (`i=0`
and `s=10`) are emitted in source order before the
test-step header — the comma's "evaluate LHS for side
effect, then RHS" semantics are the same in for-init
expression position as in expression-statement
position.

**Process note**: batch 307's first capture attempt
hung indefinitely in DOSBox (only ~11 CPU seconds in
25+ minutes) without producing OBJ output. Killing the
stuck process and re-running succeeded on the first
retry — likely an audio-init / SDL race on the WSL2
host, not a fixture-correctness issue.

## `ptr == ptr`, `fn(int a[])`, `||` short-circuit with side effect

Fixtures `1235` (`int *p=&a; int *q=&b; return p == q;`
— equality compare between two pointer values), `1236`
(`int sum(int a[]) { return a[0]+a[1]; }` — function
parameter declared with array syntax `int a[]`), and
`1237` (`int a=0; int b=5; if (a || ++b) return b;` —
the `||` RHS has a side effect on `b`) all pass on the
first capture. `1235` confirms pointer-equality lowers
identically to int-equality at the OBJ level: 16-bit
`cmp` and `sete`-style boolean materialization; the
type-checker's pointer awareness doesn't change the
emitted code. `1236` confirms `int a[]` is parsed and
treated as a synonym for `int *a` — caller passes the
array base pointer (`b` decays), callee uses subscript
on the pointer with the standard `mov bx,[bp+arg] / shl
bx,1 / add bx,...` sequence. `1237` confirms `||`
short-circuits: the RHS `++b` is only evaluated when
the LHS is zero, so we see the LHS test branch to the
RHS-evaluation block, and `b` is correctly incremented
exactly once (since `a == 0`). The body's `return b`
sees `b == 6`, confirming side-effect ordering.

## Pointer-to-pointer deref, `int = sizeof(int)`, `int += char`

Fixtures `1232` (`int **pp = &p; return **pp;` —
double-dereference of a pointer-to-pointer), `1233`
(`int n = sizeof(int); return n;` — local int
initialized from a `sizeof` expression), and `1234`
(`int a=10; char c=3; a += c; return a;` — int local
compound `+=` with a `char` RHS) all pass on the first
capture. `1232` confirms `**pp` lowers as nested loads:
`mov bx,[bp-N] / mov bx,[bx] / mov ax,[bx]` — each
indirection costs one register-temp + one load, no
fancy multi-deref fold. `1233` confirms `sizeof(int)`
constant-folds at parse time to the literal `2`, so the
init becomes a plain `mov word ptr [bp-N],2` — no
runtime computation. `1234` confirms `int += char`
promotes the RHS via `cbw` before the `add`: load char,
`cbw`, then `add word ptr [bp-N],ax` — symmetric to
`1213`'s `char += int` shape but with the narrow-type
operand on the RHS rather than the LHS lvalue.

## `if` w/o else and compound body, discarded call, `char * int` LHS

Fixtures `1229` (`if (a > 3) a *= 2; return a;` — a
single-statement compound body with no else branch),
`1230` (`f(5);` — a call whose return value is dropped
in expression-statement position), and `1231` (`char c=3;
int a=10; return c * a;` — multiplication with `char`
on LHS and `int` on RHS) all pass on the first capture.
`1229` confirms the `if-no-else` codegen: the false
branch jumps directly to the post-body label with no
synthetic empty arm. `1230` confirms call-as-statement:
the return value still lands in AX as usual, but no
store/use follows -- AX is implicitly clobbered. `1231`
is the mirror of `1228` (`int * char`): the LHS `char`
is `cbw`-promoted into AX, then the RHS `int` loads and
`imul`s -- evaluation order is left-to-right regardless
of which side is the narrow type, so the operand
loading sequence differs from `1228` but the final
encoding length is the same.

## Array-size const arith, fn returns `char *`, `int * char` RHS

Fixtures `1226` (`int a[3+2]; ... return a[4];` — array
size is a constant arithmetic expression rather than a
bare literal), `1227` (`char *greet(void) { return
"hi"; } return greet()[0];` — function returns a string
literal pointer, caller subscripts the return value),
and `1228` (`int a=10; char c=3; return a * c;` —
multiplication with `int` on LHS and `char` on RHS)
all pass on the first capture. `1226` confirms the
constant folder evaluates `3+2` to `5` during type
checking so the array gets a single 10-byte
reservation — no different from declaring `int a[5];`
at the OBJ level. `1227` confirms function-return-
through-subscript: the call returns the literal's near
pointer in AX, the subscript path uses AX as the base
register (typically moved to BX) for the byte load.
`1228` confirms `int * char` promotes the RHS to int
via `cbw`: load the char into AL, `cbw`, then `imul` —
matching what we saw for `char + int` (1213) but on the
multiply path.

## `char * char`, fn modifies param, `char *` arg write-through

Fixtures `1223` (`char a=5; char b=4; int c = a*b;
return c;` — char times char with int destination),
`1224` (`int f(int x) { x++; return x; } return f(10);`
— callee mutates its param), and `1225` (`void f(char
*p){ *p = 7; } char c=0; f(&c); return c;` — caller
passes `&c`, callee writes through the pointer arg) all
pass on the first capture. `1223` confirms char×char
goes through the standard char-to-int promotion: both
operands `cbw`'d into AX/DX, `imul dx`, store the
16-bit result into the `int` slot — no narrow-form
mul. `1224` confirms params live in the same
slot-relative frame as locals: `x++` lowers to `inc
word ptr [bp+offset]` where `[bp+offset]` is the
positional arg slot above the saved BP, and the return
re-reads from that same slot. `1225` confirms the
`void`-returning fn shape (no AX setup at return) plus
`*p = const` byte store: callee `mov bx,[bp+arg] / mov
byte ptr [bx],7`, caller stores `&c` to the slot and
reads it back after the call.

## Factorial recursion, chained sub three vars, neg `int` `>> 1`

Fixtures `1220` (`int fact(int n) { if (n<=1) return 1;
return n * fact(n-1); } return fact(4);` — recursive
function with self-call and multiply), `1221` (`int a=20,
b=5,c=3; return a-b-c;` — chained subtract across three
locals), and `1222` (`int a = -8; return a >> 1;` —
arithmetic right-shift of a negative int) all pass on
the first capture. `1220` is the factorial counterpart
to the existing `593-recursion-sum-obj`: same frame /
stack discipline, but the post-call work is `imul`
instead of `add`, exercising the multiply-of-a-call-
result path. `1221` confirms left-associativity for `-`
across three locals: LHS subtract emits its result into
AX, push, RHS local into AX, pop into DX, `sub dx,ax /
mov ax,dx` -- the same binop-via-stack-spill pattern as
batch 295's `&` of two vars. `1222` confirms that a
negative-literal source with `>>` lowers to `sar ax,1`
(arithmetic shift) rather than `shr` -- the parser
correctly threads `int` signedness through the constant
folder, even though the literal `-8` is a constant.

## Assignment as expression value, do-while var cond, stack char array for-fill

Fixtures `1217` (`int b = (a = 7) + 3; return b;` — the
inner `=` is used both for its side effect *and* for its
value), `1218` (`int i=3; do { i--; } while (i); return
i;` — do-while whose condition is a bare variable rather
than a comparison), and `1219` (`char a[5]; for(i=0;i<5;
i++) a[i] = i; return a[2];` — stack `char` array filled
by a for-loop with index store) all pass on the first
capture. `1217` confirms that assignment is treated as
an rvalue with the assigned value left in AX after the
store, so the subsequent `+ 3` can chain without
re-loading from the slot. `1218` confirms the do-while
test-on-bare-var path emits `or ax,ax / jne TOP` (the
canonical zero-test) rather than the comparison-style
`cmp / jne` we get when the condition is `i != 0`. `1219`
exercises stack-char-array element store with a runtime
index: `bx` holds the index, `mov [bp+bx-N], al`
(byte-store), matching the existing read-side path —
and confirms the for-loop counter post-step + body share
the same slot for `i` without spilling.

## Unsigned int sub, `uint < uint` as int value, `uint -= const`

Fixtures `1214` (`unsigned a=10,b=3; return a - b;` —
unsigned subtraction returned as int), `1215` (`unsigned
a=5,b=10; return a < b;` — unsigned less-than reified as
the function return value), and `1216` (`unsigned a=10;
a -= 3; return a;` — unsigned compound `-=` by const)
all pass on the first capture. `1214` confirms that
unsigned subtraction emits the same `sub` as signed (the
underlying 16-bit subtract is sign-agnostic); the unsigned
distinction only matters at the *compare* / *div* / *shr*
level. `1215` is the value-position counterpart to the
existing `175-unsigned-cmp-obj` (if-style): we see the
unsigned-aware `jb` rather than `jl` driving the
boolean-materialization sequence — so the cmp-as-int path
properly threads the signedness through. `1216` is the
unsigned analogue to int compound `-=`: identical
`sub word ptr [bp-N],3` regardless of signedness, since
the subtract itself doesn't differ at the encoding level.

## Char array elem compound `*=`, int local `+= -3`, char `+=` int RHS

Fixtures `1211` (`char a[3]; a[0]=2; a[0] *= 5; return
a[0];` — char-array element compound `*=` by a non-pow2
const), `1212` (`int a=5; a += -3; return a;` — int
local compound `+=` with a negative literal RHS), and
`1213` (`char c=5; int n=3; c += n; return c;` — char
local compound `+=` with an `int` RHS) all pass on the
first capture. `1211` confirms the char-array-elem
compound path uses the same K-threshold split as int
mul: K=5 is non-pow2, so we go through `mov dx,5 / imul
dx` rather than shifts, then narrow back to `byte ptr` on
the store. `1212` confirms that the parser/lowering
folds `+= -3` into the same emission as `-= 3` — the
unary minus on the constant is constant-folded at parse
time so we see `sub word ptr [bp-N],3`, not `add` with a
negative immediate. `1213` confirms char-with-int-RHS
promotion: the LHS `char` is `cbw`-extended to `int`,
add, then narrow back via the existing byte-store path —
matching what we already saw in the struct-field variant
(`848-struct-member-char-compound-add-int-var-obj`) for
the non-struct base case.

## Int pointer diff, string-literal subscript, int array elem compound `*=`

Fixtures `1208` (`int *p = &a[0]; int *q = &a[2]; return
q - p;` — pointer-minus-pointer yielding an element
count), `1209` (`return "abc"[1];` — subscript directly
into a string literal), and `1210` (`int a[3]; a[0]=2;
a[0] *= 5; return a[0];` — array-element compound
multiply by const) all pass on the first capture. `1208`
confirms our `int*` minus `int*` lowering: subtract the
two pointer values then `sar ax,1` (i.e. divide-by-2 for
the int element size). `1209` exercises the rarely-tested
"string literal as an addressable expression" path —
BCC emits the literal into `_TEXT` (or DGROUP for `-ms`)
with a `LDATA`-style symbol and uses the same subscript
lowering as for a `char` array. `1210` is the first
compound `*=` we've tested on an `int` stack-array elem:
the LHS lvalue is recomputed for both the load and the
store, which means the index expression must be
side-effect-free for a stable address — which it is here
since `0` is a literal. Combined with the recent
mul-pow2 K-threshold fixes, this confirms compound `*=`
on `int` array elems with a non-pow2 constant uses the
straightforward `mov dx,K / imul dx` lowering rather
than the shift form.

## For-loop summing index, stack int-array sum, nested for-loop counter

Fixtures `1205` (`for (i=0; i<3; i++) s += i;` — index
summed via compound `+=`), `1206` (`int a[3]; a[0]=1;
a[1]=2; a[2]=3; return a[0]+a[1]+a[2];` — three-elem
stack int-array sum), and `1207` (`for(i=0;i<2;i++) for
(j=0;j<2;j++) s++;` — nested for-loop with inner-body
counter) all pass on the first capture. `1205` closes the
gap for a "real" for-loop counter pattern: init / test /
post / body, with `i++` post-step lowering to `inc word
ptr [bp-N]` and the body `s += i` going through the
standard AX-spill `+= var` path. `1206` confirms our
stack int-array layout: three contiguous words, byte
offsets 0/2/4, each store via `mov [bp-N+k],ax` and the
final `+` sum reusing the same slot bases. `1207`
exercises nested-for control flow with both the inner
and outer post-step + condition test, plus a hoisted
inner test label that the parser's loop-context stack
must keep distinct from the outer's. Note: `1206`'s
first capture hit a transient DOSBox PulseAudio assert
on the verify step; rerun succeeded — the OBJ file
itself was captured cleanly. Not a codegen issue, just
audio-init flakiness on the WSL2 host.

## Ternary as discarded side effect, `!!a`, int AND of two vars

Fixtures `1202` (`int a=3; a > 0 ? a++ : a--; return a;` —
the conditional is evaluated for its side effect with the
result discarded), `1203` (`int a=5; int b = !!a; return
b;` — double-negation as a 0-or-1 normalizer), and `1204`
(`int a=0xff; int b=0x0f; return a & b;` — basic `int`
AND between two locals) all pass on the first capture.
`1202` confirms that a ternary in statement position
lowers each arm into the same branch shape we use when
the result is stored, but the arm-result is then dropped:
no AX consolidation, just the side effect. `1203` shows
that `!!a` collapses to two `cmp/sete`-style boolean
materializations stacked back-to-back rather than being
short-circuited to a single normalizer — BCC takes the
expression as written. `1204` confirms our standard
binop-via-stack-spill path for `&` on two locals: LHS
into AX, push, RHS into AX, pop into DX, `and ax,dx`.

## Int preinc result used, char-to-int cast, three-way if/else

Fixtures `1199` (`int a=5; int b=++a; return b;` — int
prefix `++` used as RHS), `1200` (`char c=5; int x=(int)c;
return x;` — explicit char-to-int cast), and `1201`
(`if (a>0) return 1; else if (a<0) return -1; else
return 0;` — three-way if/else if/else chain) all pass on
the first capture. `1199` confirms that `int b = ++a;`
lowers the same as `++a; int b = a;` — pre-increment
writes the bumped value back to the slot and leaves it in
AX in time for the subsequent store. `1200` confirms that
explicit `(int)c` lowers identically to implicit
char-to-int promotion: a `cbw` on the byte loaded into
AL, no extra cast machinery. `1201` closes a coverage gap
for chained `if/else if/else`: each `else` branch flows
through the same return-epilogue join, with the BCC
tail-merge keeping a single `pop bp / ret` at the
function exit rather than per-arm epilogues.

## Int mul by 64, int mod by var, char compound shl by two

Fixtures `1196` (`int a=3; return a*64;` — int mul by 64,
K=6 shifts), `1197` (`int a=17; int b=5; return a%b;` —
int `%` by variable), and `1198` (`char c=3; c <<= 2;
return c;` — char compound `<<=` by const 2) all pass on
the first capture. `1196` is a regression probe for the
mul-pow2 K≥4 threshold that batch 290 fixed in the
general AX path: K=6 now correctly emits `mov cl,6 / shl
ax,cl` rather than six unrolled `shl ax,1` instructions.
`1198` covers char compound `<<=` by a small constant
(K=2), which falls under the K≤3 unrolled-shift form for
the char-compound path; combined with batch 292 (which
fixed `*= 16` to mirror the K≤3 / K≥4 split), this
confirms the char-compound shift/mul threshold is now
consistent with the general AX path. `1197` confirms our
`int % var` lowering still matches: `cwd / idiv bx` with
the remainder coming out of DX.

## Int mul by 256, char compound mul by 16, int init deref+add

Fixtures `1193` (`int a=2; return a*256;` — int mul
by 256, K=8 shifts), `1194` (`char c=3; c *= 16;
return c;` — char compound mul by 16, K=4 shifts),
`1195` (`int a=10; int *p=&a; int b = *p+5; return
b;` — initialize an int from a pointer-dereference
plus a constant).

1193 and 1195 already worked after the batch-290
fix. 1195 uses `mov bx, [bp-Np]; mov ax, [bx];
add ax, 5; mov [bp-Nb], ax` — no extra address
materialization since the int-init path can take an
AX-resident value directly.

1194 caught the analogous bug in the char compound
`*=` paths — both the local (`reg.is_byte()`
branch in `emit_compound_with_value`) and the
global (`Type::Char | Type::UChar` branch in the
global-compound dispatch) unconditionally unrolled
all shifts, missing the K ≥ 4 → CL form. Fixed by
mirroring the same threshold in both arms. Spot-
checked all 15 char/long compound `*=` fixtures
(`633`, `672`, `690`, `693`, `695`, `741`, `747`,
`762`, `772`, `781`, `785`, `786`, `817`, `831`)
— all still match since their K values are ≤ 3
shifts.

## Int mul by 32, uint mul by 16, int deref RMW

Fixtures `1190` (`int a=3; return a*32;` — int mul
by 32, K=5 shifts, exercising the CL-form path
added in batch 290), `1191` (`unsigned int u=5;
return u*16;` — unsigned int mul by 16, K=4 shifts,
confirming the threshold path is signedness-
agnostic), `1192` (`int a=5; int *p=&a; *p =
*p + 1; return a;` — read-modify-write through a
pointer, both LHS and RHS go through the same
deref).

All three already worked end-to-end after the
batch-290 mul-pow2 fix. 1190 and 1191 emit `mov cl,
N; shl ax, cl` for K ≥ 4 shifts regardless of
operand signedness — `imul` and `shl` produce the
same low 16 bits whether the operand is treated as
signed or unsigned, so BCC doesn't distinguish on
the mul-pow2 path. 1192 emits `mov bx, [bp-Np];
mov ax, [bx]; inc ax; mov bx, [bp-Np]; mov [bx],
ax` — BCC reloads `p` into BX rather than caching it
across the increment, since the LHS and RHS are
independent sub-expressions in the AST and each
gets its own address materialization.

## Int mul by 16, int div by var, int store through ptr

Fixtures `1187` (`int a=5; return a*16;` — int mul
by a power-of-two larger than 8, exercising the
unroll-vs-CL threshold), `1188` (`int a=20; int
b=4; return a/b;` — int divide by a variable),
`1189` (`int a=1; int *p = &a; *p = 99; return a;`
— store through a pointer to a local).

1188 and 1189 already worked. 1188 uses the
standard `cwd; idiv <mem>` form against the memory
operand — variable RHS goes through the existing
`emit_op_with_source` mem-form. 1189 emits `lea bx,
[bp-Na]; mov [bp-Np], bx` for the address-of init,
then `mov bx, [bp-Np]; mov word ptr [bx], 99` for
the deref-store.

1187 caught a real codegen bug: our mul-by-pow2
path in `emit_op_with_source` always unrolled to
N×`shl ax, 1`, ignoring the K≤3 unroll threshold
that already governs explicit-shift expressions
(see fixtures 110/627). For `*16` (K=4 shifts)
this produced 8 bytes (4× `shl ax, 1`) vs BCC's
4 bytes (`mov cl, 4; shl ax, cl`). Fixed by mirroring
the shift threshold inside the mul-pow2 arm: shifts
≤ 3 keep the unroll, shifts ≥ 4 emit the CL form.
Spot-checked the existing mul-pow2 fixtures (1137
`*8`, 283 `long*2`, 550, 592, 602, 645, 853) — all
still match since their K values are ≤ 3 shifts.

## Int add three distinct, int multi-init stmt, char ge-cmp in if

Fixtures `1184` (`int x=1; int y=2; int z=4;
return x+y+z;` — sum of three distinct-named locals
as a single return expression), `1185` (`int a=1,
b=2; return a+b;` — multi-declarator statement with
each declarator carrying its own initializer, sibling
of 1151 which was the bare-uninit-declarators form),
`1186` (`char c=5; char d=3; if (c >= d) return 1;
return 0;` — signed char `>=` compare used as an if
condition rather than a value).

All three already worked end-to-end. 1184 reuses
the sum-three-locals fold from 1151 even though the
locals have different names. 1185's parser path
processes each declarator's initializer at the
declaration site rather than lazily — so `a=1` and
`b=2` each emit `mov word ptr [bp-N], imm` directly,
matching the equivalent two-statement form. 1186
widens both chars via `mov al, byte ptr <c>; cbw`
(then push/pop because the second char also needs
widening) then dispatches the signed `jge`/`jl`
branch — char compares in if/while context use
signed jumps per the batch-181/187 promote-to-signed-
int rule we documented earlier.

## Int shr then mask, while multi-stmt, int assign-then-mul

Fixtures `1181` (`int a=0x123; int x = (a>>4) &
0xf; return x;` — extract-nibble pattern, shift then
mask), `1182` (`int i=0; int s=0; while (i<4) { s
= s + i; i = i + 1; } return s;` — while loop with
a compound body containing two assignments), `1183`
(`int a=3; int b=4; int x; x = a+b; return x*2;` —
uninitialized declaration followed by an assignment,
then the value is reused in a different statement).

All three already worked end-to-end. 1181 emits the
straight `mov ax, [bp-Na]; mov cl, 4; shr ax, cl;
and ax, 15` sequence — both halves of the
extract-nibble compose cleanly in AX without spill
since the mask is an immediate. 1182's while body
is a brace-block compound that the loop lowering
already handles — emit each statement in sequence
between the top label and the back-jump. 1183
confirms the locals planner correctly tracks the
declared-but-not-initialized `x` slot (no init
emitted at the declaration site) and then the
subsequent assignment uses the same word-store path
as any other int assign.

## Int and-const-one, uint shr by const, int deref then add

Fixtures `1178` (`int a=7; int x = a & 1; return
x;` — low-bit isolate via AND with constant 1),
`1179` (`unsigned int u=100; return u>>2;` — unsigned
int right-shift uses `shr` rather than `sar`, the
unsigned-versus-signed dispatch hinging on the
operand type), `1180` (`int a=5; int *p = &a;
return *p + 1;` — deref through a pointer-to-local
then add a constant).

All three already worked end-to-end. 1178 emits the
canonical `mov ax, [bp-Na]; and ax, 1` and stores
the result. 1179 confirms BCC dispatches on operand
signedness for shifts in value context the same way
it does in compound context: `mov ax, [bp-Nu]; shr
ax, 1; shr ax, 1` (K=2 → individual single-bit
shifts, matching the batch-110 K≤3 unroll
threshold). 1180 emits the LEA-into-BX path: `lea
bx, [bp-Na]; mov [bp-Np], bx; ... mov bx, [bp-Np];
mov ax, [bx]; inc ax` for the deref-then-add.

### Deferred from batch 287

- Probed `int a=5; int b=3; int r = !(a > b); return
  r;` (`1178` first draft). 5-byte diff. BCC fuses
  `!cmp` by inverting the jump (`jg` rather than
  `jle`) so the boolean materialization produces the
  inverted result directly: cmp, jg-to-zero-arm,
  `mov ax, 1`, jmp, `xor ax, ax`. Our codegen
  materializes the cmp as a normal 0/1 boolean and
  then applies `!` via the generic `neg ax; sbb ax,
  ax; inc ax` sequence (5 bytes), unaware that the
  operand is itself a compare result that could have
  emitted the inverted condition for free. The fix
  is a `UnaryNot(Compare(...))` peephole in
  `emit_expr_to_ax` that calls the boolean-
  materialization helper with the inverted
  jump-condition. Probe replaced with the
  AND-const-1 variant until that peephole lands.

## Int ne-zero as int, if-or-of-cmps, int mod pow2

Fixtures `1175` (`int a=7; int r = a!=0; return r;`
— int compared to literal zero with `!=`, sibling
of 1172's `==0`), `1176` (`int a=0; int b=7; if
(a>0 || b>0) return 1; return 0;` — short-circuit
`||` of two int compares, sibling of 1174's `&&`),
`1177` (`int a=17; return a%2;` — int modulo by 2,
the smallest power-of-2 constant).

All three already worked end-to-end. 1175 uses the
boolean-materialization sequence with `jne`. 1176
short-circuits via two `cmp; jg` pairs: the first
success jumps directly to the true-arm body, the
second failure falls through to the false arm — the
`||` lowering is the dual of `&&`. 1177 confirms that
unlike `c /= 8` on char (which collapses to `sar`,
fixture 1153), int `% pow2` does **not** get a
mask-with-AND optimization — BCC still emits the full
`cwd; mov cx, 2; idiv cx` sequence and returns DX.
For unsigned int the AND would be valid; for signed
it isn't, so this is consistent with BCC keeping the
signed-int divide pessimistic.

## Int eq-zero as int, int shl-then-or-const, if-and-of-cmps

Fixtures `1172` (`int a=0; int r = a==0; return r;`
— int compared to literal zero materialized as int,
sibling of the 1159 char==0 case), `1173` (`int
a=0x12; int x = (a << 8) | 0xff; return x;` — shift
then OR with a constant rather than another variable),
`1174` (`int a=5; int b=7; if (a>0 && b>0) return 1;
return 0;` — short-circuit `&&` of two int compares
in an if condition).

All three already worked end-to-end. 1172 uses the
boolean-materialization sequence with `cmp ax, 0`
followed by the `je` arm. 1173 emits `mov ax, [bp-Na];
mov cl, 8; shl ax, cl; or ax, 255` — the right-hand
side being an immediate avoids the
register-allocation issue documented below. 1174
short-circuits via two `cmp; jle` pairs to the
fall-through label — the `&&` lowering does the first
compare, falls through on success to the second
compare, and uses the same fall-through label for
both failure jumps.

### Deferred from batch 285

- Probed `int a=0x12; int b=0x34; int x = (a & 0xff)
  | (b << 8); return x;` (`1173` first draft). 1-byte
  diff. BCC reorders the binop so the shift-needing
  operand is computed first into AX, then loads the
  other operand into DX with `mov dx, [bp-Na]; and
  dx, 0xff` (longer encoding because not AX), and
  finishes with `or dx, ax; mov [bp-Nx], dx` — keeping
  both operands in registers across the OR with no
  spill. Our codegen still pushes AX, computes the
  other side into AX, pops to DX, then ORs. To match
  we'd need a binop-via-DX path that picks register
  vs. spill based on whether the simpler side can be
  evaluated without clobbering. Probe replaced with
  the `(a << 8) | 0xff` shape (immediate RHS, no
  cross-operand register pressure) until we land that
  allocator change.

## Do-while counter, int mask then shl, int lt-const as int

Fixtures `1169` (`int i=0; do { i++; } while (i<3);
return i;` — minimal do-while loop with a counter,
sibling of the 1158 while-counter shape), `1170`
(`int a=0x123; int x = (a & 0xff) << 4; return x;` —
mask-then-shift composition with hex constants),
`1171` (`int a=5; int r = a<10; return r;` —
compare-against-const variant of the lt-cmp family,
sibling of 1166 where both sides were variables).

All three already worked end-to-end. 1169 emits a
top-label, body, then conditional `jl` back to the
top — the do-while shape skips the entry-condition
test and falls into the body once unconditionally
(slot layout has only the top label, no fall-through
exit slot). 1170 evaluates `(a & 0xff)` into AX with
`and ax, 255` and then `shl ax, 4` (CL form per the
batch-110 threshold: K=4 → CL). 1171 swaps the
variable RHS for an `imm16` in the compare —
`cmp ax, 10` rather than `cmp ax, [bp-Nb]`.

## Int lt-cmp as int, int gt-cmp as int, comma op in init

Fixtures `1166` (`int a=3; int b=5; int r = a<b;
return r;`), `1167` (`int a=5; int b=3; int r =
a>b; return r;`), `1168` (`int a=0; int b = (a=1,
a+2); return b;` — comma operator as the initializer
expression: side-effect the LHS (assign a), discard,
then take the RHS value as the init value).

All three already worked end-to-end. 1166/1167 complete
the signed compare-as-int family alongside `==/!=/<=/>=`
(1149/1159 and 1160/1163) using the matching `jl`/`jg`
arms. 1168 reuses the existing comma-expression
lowering: the LHS is emitted via `emit_expr_discard`
(so `a = 1` writes to a's slot but doesn't leave a
result in AX), then the RHS `a + 2` is evaluated into
AX and the int-init store writes it to b.

## Int le-cmp as int, int shl by var, int mul by three

Fixtures `1163` (`int a=3; int b=5; int r = a<=b;
return r;` — signed `<=` materialized to int 0/1,
sibling of 1160's `>=`), `1164` (`int a=4; int b=3;
int x = a<<b;` — int left-shift by a variable count
via CL, sibling of 1162's right-shift), `1165` (`int
a=7; return a*3;` — int multiply by the small odd
constant 3).

All three already worked end-to-end. 1163 uses the
boolean-materialization sequence with the signed `jle`
arm. 1164 widens `b` through CX and emits `shl ax,
cl`. 1165 emits the standard `mov ax, [bp-Na]; mov
cx, 3; imul cx` — BCC does **not** lower `* 3` to
`lea ax, [bx+bx*2]` or `mov dx, ax; shl ax, 1; add
ax, dx`; it always reaches for `imul` once the constant
isn't a power of two, even for tiny constants like 3
or 5.

## Int ge-cmp as int, int chained sub const, int shr by var

Fixtures `1160` (`int a=5; int b=3; int r = a>=b;
return r;` — signed `>=` materialized to int 0/1),
`1161` (`int a=10; int b=3; int x = a-b-1;` —
left-associative `a-b-1` chained subtraction), `1162`
(`int a=32; int b=2; int x = a>>b;` — int signed
right-shift by a variable count via CL).

All three already worked end-to-end. 1160 reuses the
batch-280 boolean-materialization sequence with the
signed `jge` arm. 1161 emits `mov ax, [bp-Na]; sub
ax, [bp-Nb]; sub ax, 1` then stores AX into the local
slot. 1162 loads `b` into CL via `mov cx, [bp-Nb]`
(BCC widens through CX) and emits `sar ax, cl` against
the AX-loaded `a`.

### Deferred from batch 281

- Probed `char c = 7; char b = c--; return b;` (`1161`
  first draft). Our char-local-init dispatch panics
  with `non-constant char local init shape not yet
  supported` for the `Postfix(Dec)` source-expr kind —
  it currently recognizes only `Ident`, the `(char)`-
  cast peephole, char-binop arith, char-shift-by-const,
  and Dot-Member chains. BCC for this shape also
  enregisters `c` (it lives in DL across the function,
  not on the stack) which would need locals-planner
  cooperation, not just a new init arm. Probe replaced
  with the int chained-sub variant until we tackle
  byte-register enregistration.

## Int OR of shift and val, while counter to three, char eq zero as int

Fixtures `1157` (`int a=3; int b=5; int x=(a<<4)|b;
return x;` — bitwise OR of a left-shifted value and a
local, the classic nibble-packing pattern), `1158`
(`int i=0; while (i<3) i++; return i;` — minimal
while-loop with a counter), `1159` (`char c=0; int r=
(c==0); return r;` — char==0 compare whose boolean
result is stored into an int local).

All three already worked end-to-end. 1157 emits AX-load
of `a`, `shl ax, 4`, then `or ax, [bp-Nb]` before
storing into `x`. 1158 uses the standard while-shape
(`@1:` top label, body, jump back) and `inc word ptr
<i>` for the increment. 1159 widens the char load to
AX with `mov al, byte ptr <c>; cbw`, compares to 0,
and uses the same boolean-materialization sequence as
the int `!=` path (1149) but the equal-arm.

## Int postinc as RHS, int mod by const, conditional as RHS

Fixtures `1154` (`int a=5; int b=a++; return a+b;` —
post-increment used as an initializer expression so the
pre-value flows into `b` and `a` then carries the
incremented value), `1155` (`int x=17; return x%5;` —
int modulo by a non-power-of-two constant), `1156`
(`int r = (a > b) ? a : b;` — ternary conditional used
as the initializer of a local).

All three already worked end-to-end. 1154 uses the
existing postinc-as-value path: load `a` into AX, store
to `b`'s slot, then increment the source slot in place.
1155 emits the standard `cwd; mov cx, 5; idiv cx` and
returns DX. 1156 reuses the existing ternary-to-AX
lowering and stores the merged AX into the local slot
via the regular int-init store.

### Deferred from batch 279

- Probed `char s[5] = {1, 2, 3, 4, 5}; return s[0] +
  s[4];` (`1155` first draft). Our codegen panics at
  `non-constant init for non-int-like type Array { elem:
  Char, len: 5 } not yet supported` — the stack-local
  init-list path is only wired for scalar types; the
  array+InitList shape needs a per-element store
  sequence (analogous to the global-array path that
  exists for fixtures 526 and 567 but emits into `[bp-
  N+i]` for stack slots). Probe replaced with the int-
  modulo variant until we add a stack-array init-list
  lowering.

## Int multi-decl statement, deref of ptr plus 1, char compound div pow2

Fixtures `1151` (`int a, b, c; a=1; b=2; c=3; return
a+b+c;` — single declaration statement with three
comma-separated declarators), `1152` (`int a[3]; int
*p = a; a[1] = 77; return *(p + 1);` — deref of a
pointer-plus-constant expression rather than the array-
subscript form), `1153` (`char c = 16; c /= 8;
return c;` — char compound divide by a power-of-two
constant).

All three already worked end-to-end. 1151's parser
already lowered a comma-separated declarator list to
three independent locals so each `a=1`/`b=2`/`c=3`
assignment uses the per-slot int store and the
sum-three-locals add fold applies. 1152's `*(p+1)`
parses as `Unary(Deref, Binary(Add, p, 1))` which the
codegen already routes through the same scaled-pointer
load that drives `p[1]`: BCC emits the `bx`-based
`mov ax, [bx+2]` form. 1153 confirms the char-compound
`/=` const path already collapses a power-of-two divisor
to a right-shift rather than going through `idiv` —
`c /= 8` emits as `sar` by 3 on the promoted byte (same
sign-rule as the shift path); no byte-`idiv` was needed.

## Long compound add var, int return ne as value, neg of bitwise NOT

Fixtures `1148` (`long g = 100L; long x = 5L; g += x;
return (int)g;` — long global compound add by a local
long var RHS), `1149` (`int a=5; int b=5; return a !=
b;` — int return of != compare with the boolean result
materialized as 0 or 1), `1150` (`int x = 5; return
-~x;` — int return of negation of bitwise complement,
the identity `-~x == x+1`).

All three already worked end-to-end. 1148 uses the
long compound add-with-carry path. 1149 emits the
compare-as-value sequence with the `jne`/`xor ax,ax`
boolean materialization. 1150 emits `mov ax, [bp-N];
not ax; neg ax`.

## Int swap via temp, global long neg init, int sub-then-add

Fixtures `1145` (`int a=1; int b=2; int t; t=a; a=b;
b=t;` — classic three-step swap exercising reg-to-mem
and mem-to-reg copies between register and stack
locals), `1146` (`long g = -1000L; return (int)g;` —
global long with negative init), `1147` (`int a = 10;
int b = 3; int c = 2; return a - b + c;` — left-
associative sub-then-add chain).

1146 and 1147 already worked end-to-end.

1145 exercised two missed peepholes simultaneously:
`t = a` (reg-to-mem) emitted `mov ax, si; mov [bp-N],
ax` instead of BCC's `mov [bp-N], si`; `b = t` (mem-
to-reg) emitted `mov ax, [bp-N]; mov di, ax` instead
of `mov di, [bp-N]`.

Added two siblings of the batch-275 reg-to-reg
peephole:

- **Mem-to-reg in `emit_store_reg`**: when the RHS is a
  bare-ident naming a stack-resident plain `int`
  local, emit `mov <reg>, word ptr [bp-N]` directly.
- **Reg-to-mem in `emit_assign_local`**: when both the
  destination and the RHS are plain `int` locals (dest
  on stack, RHS in a register), emit `mov word ptr
  [bp-N], <reg>` directly.

Both are restricted to `Type::Int` exact match to
avoid affecting pointer/array/char/long paths that
have their own decay or widening sequences (a too-
broad initial filter incorrectly matched stack-array-
ident sources, breaking the array-decay-to-pointer
shape — narrowed before commit).

## Global int compound add var, int reg-to-reg assign, global char xor const

Fixtures `1142` (`int g = 10; int x = 5; g += x;
return g;` — global int compound add by a variable
RHS), `1143` (`int x = 1; int y = 2; x = y; y = 99;
return x;` — int reg-to-reg copy when both locals are
register-resident), `1144` (`char g = 0x0F; g ^= 0x05;
return g;` — global char compound XOR by constant).

1142 and 1144 already worked end-to-end via the
existing memory-direct compound paths.

1143 emitted an unnecessary AX round-trip. With both
x and y register-resident (SI and DI), our `x = y;`
went `mov ax, di; mov si, ax` (4 bytes total). BCC
emits the direct reg-to-reg form: `mov si, di` (2
bytes).

Added a reg-to-reg peephole to `emit_store_reg`: when
the RHS is a bare-identifier naming another register-
resident int local (both 16-bit), emit `mov <dest>,
<src>` directly. Byte registers stay on the
fall-through path.

## Long global shr by const, ternary two consts, struct field from field

Fixtures `1139` (`long g = 1024L; g >>= 2; return
(int)g;` — long global compound shift-right by
constant), `1140` (`int x = 5; return x > 0 ? 100 :
-1;` — ternary in return position with two int
constant arms), `1141` (`s.x = 42; s.y = s.x; return
s.y;` — struct field assigned from another field of
the same struct).

All three already worked end-to-end. 1139 uses the
long-shift helper. 1140's ternary picks one of two
constants based on the compare. 1141 does the field-
to-field copy through AX.



Fixtures `1136` (`int x = 64; x = x >> 2; return x;`
— int compound shift as assign statement), `1137`
(`int x = 5; return x * 8;` — int multiply by a power-
of-2 constant), `1138` (`int x = 5; int *p = &x; if (p
== 0) return 1; return 0;` — pointer compared to null
in if-condition).

All three already worked end-to-end. 1136 lowers `x =
x >> 2` as `mov ax, [bp-N]; sar ax, 1; sar ax, 1; mov
[bp-N], ax` (K=2 unroll). 1137 uses the power-of-2
shift peephole: `mov ax, [bp-N]; shl ax, 1; shl ax,
1; shl ax, 1`. 1138 emits the existing pointer-cmp-
zero peephole.

**Recorded findings (deferred):**

- Probed `int g[3] = {...}; int i = 2; return g[i];`
  as fixture 1136 first draft. Panic: "variable-
  indexed global array not yet supported". The
  global-array variable-index read path is unwritten —
  the global-array-read codegen today expects a const
  index. Sibling of the existing local-array-variable-
  index path.
- Probed `char c = 5; c *= 3; return c;` as fixture
  1137 first draft. Our codegen emits `imul al, 3`
  which the assembler rejects with "unsupported
  operand form `3`" — 8086 has no `imul reg8, imm8`
  encoding; the byte path must go via the AX form
  (`mov al, 3; imul al`) or widen to int and use
  `imul reg, imm`. Char-compound-mul-by-imm needs a
  distinct lowering.
- Probed `int a[3]; int i; for (i=0; i<3; i++) a[i]
  = i;` as a follow-up. Panic: "non-constant rhs in
  variable-indexed array assign not yet supported".
  Sibling of the variable-indexed read deferral; the
  write path with a non-constant RHS isn't wired up.



Fixtures `1133` (`return 32767;` — return of i16 max
positive literal), `1134` (`char s[3]; s[0]='X'; s[1]
='Y'; s[2]='Z'; return s[1];` — stack char array
with three byte stores and a read), `1135` (`int g =
10; --g; return g;` — global int pre-dec as statement
followed by a return).

All three already worked end-to-end. 1133's literal
folds to imm16 0x7FFF. 1134 emits three `mov byte ptr
[bp-N+K], imm8` stores. 1135 uses `dec word ptr DGROUP:
_g` directly.



Fixtures `1130` (`int a = 0xA; int b = 0xC; return a ^
b;` — int return of XOR of two int locals), `1131`
(`int x = 0xFF; return x & 0x0F;` — int return of AND
with a constant mask), `1132` (`if (a > 0) { if (b >
0) return 1; } return 0;` — nested if with bracketed
body).

All three already worked end-to-end. 1130 lowers `a ^
b` as `mov ax, [bp-Na]; xor ax, [bp-Nb]`. 1131 uses
the accumulator form `and ax, 0x0F`. 1132 emits the
two nested conditional branches with separate label
slots.



Fixtures `1127` (`int a = 1, b = 2, c = 3; int r = a +
b + c; return r;` — three-way int sum stored into a
local before return), `1128` (`int g[3] = {-1, -2,
-3}; return g[0] + g[1] + g[2];` — global int array
with negative initializer values), `1129` (`int a = 7;
int b = 3; int c = 5; return a * b - c;` — return of
mul-then-sub with three int locals).

All three already worked end-to-end. 1127 and 1129
exercise the int-binop chain (add-add and mul-sub).
1128's negative-init stores each value as its
unsigned-wrapped i16 form (`-1` → 0xFFFF, etc.) in the
`dw` directive.

**Recorded finding (deferred):**

- Probed `struct S { char c; }; struct S *p = &s; char
  b = p->c; return b;` as fixture 1127 first draft.
  Hit the char-init panic — the batch-269 peephole
  handles `Dot`-kind Member sources but not `Arrow`.
  The Arrow form needs `mov bx, word ptr [bp-Np];
  mov al, byte ptr [bx+field_off]; mov byte ptr
  <dest>, al`, not the compile-time-folded address of
  the Dot path. Deferred until a fixture forces the
  pointer-dereferenced char-init shape.



Fixtures `1124` (`struct S { char c; }; struct S s =
{'Q'}; char b = s.c; return b;` — char init from a
struct char member, sibling of fixture 1115's assign
form), `1125` (`int g = 20; g -= 5; return g;` —
global int compound sub by imm8 constant), `1126`
(`int g = 42; int *p = &g; return *p;` — pointer init
from global address, then return via deref).

1125 and 1126 already worked end-to-end. 1125 uses
the memory-direct form `sub word ptr DGROUP:_g, 5`.
1126's `&g` lowers as `mov si, offset DGROUP:_g`; the
`*p` deref then emits `mov ax, word ptr [si]`.

1124 hit the char-init panic — the existing arms
handled `Cast`/`Ident`/`BinOp`/`Shr`/`Shl` source
shapes but not `Member`. Added a Member arm mirroring
the batch-266 assign-from-Member peephole: when the
init's RHS is a `Dot`-kind `Member` whose leaf type
is char-like, emit `mov al, byte ptr <field-addr>;
mov byte ptr <dest>, al` directly. Both global and
stack struct sources handled.



Fixtures `1121` (`struct S { int x; }; struct S arr[2];
arr[0].x = 5; arr[1].x = 7; return arr[0].x + arr[1].x;`
— struct array element access with field assignment
and read), `1122` (`char c = 20; c /= 4; return c;` —
char compound div by constant), `1123` (`int g[3] =
{10, 20, 30}; return g[1];` — return of global int
array element).

All three already worked end-to-end. 1121 lays out
arr[2] as a stack region of 4 bytes (2 structs × 2
bytes each), with `arr[0].x` at `[bp-4]` and `arr[1].x`
at `[bp-2]`. 1122's char `c /= 4` lowers via the
existing char-compound div path. 1123 emits `mov ax,
word ptr DGROUP:_g+2`.



Fixtures `1118` (`char c = 16; c >>= 2; return c;` —
char compound shift-right by constant), `1119` (`int g
= 10; g += 7; return g;` — global int compound add by
imm8 constant), `1120` (`int g = 7; return ~g;` —
bitwise NOT applied to a global int).

All three already worked end-to-end. 1118 follows the
byte-width compound-shift path with K=2 picking the
two-instruction unroll. 1119 uses the memory-direct
form `add word ptr DGROUP:_g, 7`. 1120 emits `mov ax,
word ptr DGROUP:_g; not ax`.



Fixtures `1115` (`struct S { char c; }; s.c = 'Z'; b
= s.c; return b;` — char local assigned from a char
struct member, closing the deferred char-from-Member
finding from batch 257), `1116` (`int x = 10; int y =
7; x += y; return x;` — int compound add-assign with
variable RHS), `1117` (`int a = 0x10; int b = 0x04;
return a | b;` — int return from bitwise OR of two
stack locals).

1116 and 1117 already worked end-to-end. 1116 uses
the standard int compound add path (`add word ptr [bp-
N], <src>`); 1117 lowers `a | b` as `mov ax, [bp-Na];
or ax, [bp-Nb]`.

1115 was the deferred char-assign-from-Member case.
Our fall-through routed through `emit_expr_to_ax`
which calls `emit_member_to_ax`, which always widens
the byte load to int via `cbw` (because the int-
promotion path expects it). For a char destination
that widen is wasted — the byte store truncates back
anyway.

Added a peephole in `emit_assign_local`'s char path:
when the RHS is a `Dot`-kind `Member` whose leaf type
is char-like (resolved via `try_member_dot_chain`),
emit `mov al, byte ptr <field-addr>; mov byte ptr
<dest>, al` directly without the cbw. Both global and
stack struct sources are handled. Sibling of the
existing char-array-elem peephole.



Fixtures `1112` (`int x = 3; x += 5; return x;` — int
compound add followed by a return that picks up the
updated value), `1113` (`return (a + b) * c;` — int
return with parens forcing addition before
multiplication), `1114` (`return (a = 7, b = 11, a +
b);` — comma operator chain with two assignments and a
final value).

All three already worked end-to-end. 1112 uses the
existing compound-add and then a separate load for the
return. 1113's `(a + b) * c` evaluates the parenthesized
add first, pushes its result, then loads `c` and
multiplies. 1114's comma chain executes the side-effect
assigns in order, with the final `a + b` becoming the
comma value returned.



Fixtures `1109` (`char c = 3; c <<= 2; return c;` —
char compound shift-left by constant), `1110` (`long g
= 100000L; return (int)g;` — global long initializer
with a value > 0xFFFF that requires both halves to
hold non-zero bits), `1111` (`int x = 5; x = x + x;
return x;` — int reassign from self-double).

All three already worked end-to-end:

- 1109: char-compound-shl-const path uses the byte-
  width form: `shl byte ptr [bp-N], 1` repeated K
  times.
- 1110: long global init splits the 32-bit constant
  into two `dw` directives at the symbol's address.
  100000 = 0x186A0; low=0x86A0, high=0x0001.
- 1111: `x + x` lowers as `mov ax, [bp-N]; add ax,
  [bp-N]; mov [bp-N], ax` — no aliasing concern, both
  loads see the same value.



Fixtures `1106` (`if (a > 0 || b > 0) return 1;` —
short-circuiting `||` of two compares as if-condition,
sibling of fixture 1104's `&&`), `1107` (`int x = 42;
return -x;` — int return of negation of a stack local),
`1108` (`int x = 128; x >>= 3; return x;` — int
compound shift-right by a constant K=3).

All three already worked end-to-end. 1106's `||`
generates the same kind of short-circuit graph as `&&`
but with the LHS-true result skipping the RHS. 1107
emits `mov ax, [bp-N]; neg ax`. 1108 picks the K ≤ 3
unroll: `sar word ptr [bp-N], 1` repeated three times.



Fixtures `1103` (`a ^= b;` — char compound XOR-assign
with char-var RHS), `1104` (`if (a > 0 && b > 0)
return 1;` — short-circuiting `&&` of two compares as
an if-condition), `1105` (`a &= b;` — char compound
AND-assign with char-var RHS).

All three already worked end-to-end. 1103/1105 round
out the char-compound permitted set alongside the
add/sub/or covered earlier (fixtures 1094/1097/1102).
1104's `&&` lowers via the existing short-circuit
control-flow path: evaluate LHS compare with
fall-through to the RHS compare, both jumping to a
common "false" label on falsy result.



Fixtures `1100` (`int g[3] = {1, 2, 3}; return g[0] +
g[1] + g[2];` — global int array initializer with
multi-element sum), `1101` (`int x = 7; int *p = &x;
*p = 99; return x;` — int pointer to a stack-local
with deref-write through the pointer), `1102` (`char a
= 1; char b = 4; a |= b; return a;` — char compound OR
with char-var RHS).

All three already worked end-to-end. 1100's array
initializer lays out as three word literals at `_g`,
and the three reads use direct `mov ax, word ptr DGROUP:
_g+K`. 1101 emits `lea ax, [bp-N]; mov si, ax` for the
address, then `mov word ptr [si], 99` for the deref-
write. 1102 follows the char compound bitwise path.

**Recorded finding (deferred):**

- Probed `int a[3]; int n = 1; int *p = a + n; a[1] =
  42; return *p;` as fixture 1101 first draft. We emit
  `lea ax, [bp+base]; add ax, [bp+n]; mov si, ax` —
  forgetting to scale `n` by sizeof(int) = 2. BCC's
  correct sequence is `mov ax, [bp+n]; shl ax, 1; lea
  dx, [bp+base]; add ax, dx; mov si, ax`. Same stride
  bug as the constant-K case (batches 243/249), but
  with a runtime-variable offset that needs the shl.
  Sibling fix: detect `<array> + <ident-int>` in the
  pointer-init path, emit the shift-and-add sequence.



Fixtures `1097` (`char a = 20; char b = 5; a -= b;
return a;` — char compound sub-assign with a char-var
RHS, sibling of fixture 1094's add form), `1098`
(`char c = -5; return c;` — char init from a negative
int literal that fits in the byte width), `1099` (`int
x = 100; int y = 3; return x / y;` — int division by
a variable RHS in return position).

All three already worked end-to-end:

- 1097: char compound `-= b` lowers via the standard
  char-compound path: `mov al, <a>; sub al, <b>; mov
  <a>, al`. Already covered.
- 1098: `-5` constant-folds to 0xFB at parse time, then
  the char-init constant path emits `mov byte ptr
  [bp-N], 251` (the unsigned-wrapped byte value).
  Already covered.
- 1099: `x / y` lowers to `mov ax, [bp-Nx]; cwd; idiv
  word ptr [bp-Ny]` then returns AX. The div-by-var
  path was added in slice 200's idiv arm.



Fixtures `1094` (`char a = 10; char b = 3; a += b;
return a;` — char compound add-assign with a char-var
RHS), `1095` (`char c = 16; return c >> 1;` — int
return from char-shifted-by-const expression in return
position), `1096` (`int a[5]; a[0] = 1; return sizeof
a;` — sizeof of a stack array that's actually used at
runtime, defeating any frame-elision quirk).

All three already worked end-to-end:

- 1094: char compound `+= b` on a stack char-local
  uses the standard char-compound path: `mov al, <a>;
  add al, <b>; mov <a>, al`. Already covered.
- 1095: `c >> 1` in return position widens via cbw
  then shifts the int value, then returns AX. The
  shift result is the int-promoted value, not the
  byte-truncated form — different from the char-init
  shift path (batch 255) where the dest is char.
- 1096: `sizeof a` where `a` is `int a[5]` folds to
  10 at parse time, and the frame is allocated for
  the runtime writes anyway, so no elision applies.
  No divergence.

**Recorded finding (deferred):**

- Probed `int a[5]; a[0] = 1; return sizeof a[0];` as
  fixture 1095 first draft. The parser doesn't accept
  `sizeof a[0]` (the `a[0]` operand form for `sizeof`)
  — only `sizeof(<type-name>)` is wired up. Adding the
  expression-operand form would need a new grammar
  branch in the unary parser plus type-of-expression
  resolution for the result.



Fixtures `1091` (`struct S { char c; }; s.c = 'Z';
return s.c;` — return of a struct char field directly,
exercising widening from member-byte-read to int return
value), `1092` (`char g = 'B'; int main() { return g; }`
— global char init and read, the simplest cross-section
of global-data + char-return), `1093` (`int x = 5; int
y = 3; x *= y; return x;` — int compound mul-assign by
a stack variable RHS).

All three already worked end-to-end:

- 1091: return-int-of-char widens via `mov al, byte
  ptr [bp-N]; cbw` (the *return* path expects the cbw
  since the return is int).
- 1092: global char `g` is stored at `_g`, read via
  `mov al, byte ptr DGROUP:_g; cbw` for the int return.
- 1093: `x *= y` lowers via the batch-111 `imul <mem>`
  path: `mov ax, [bp-Nx]; imul word ptr [bp-Ny]; mov
  [bp-Nx], ax`. Already covered.



Fixtures `1088` (`int x = 7; return x * 3;` — int local
multiplied by a non-power-of-2 constant), `1089` (`int
a[3]; int v; a[0] = 5; v = a[0] + 100;` — int assign
from array-elem-plus-const, exercising the standard
load-plus-const path), `1090` (`int a[3]; int i = 1;
... return a[i];` — return of stack-array element with
runtime-index variable).

All three already worked end-to-end. 1088 uses
`imul` with an int constant; 1089 emits `mov ax, [bp-
Na0]; add ax, 100; mov [bp-Nv], ax`; 1090 uses the
variable-index array path that loads BX and uses
`mov ax, [bx+bp+base]`.

**Recorded findings (deferred):**

- Probed `int x; return sizeof x;` as fixture 1088
  first draft. BCC ELIDES the frame allocation for `x`
  because the local is referenced only in `sizeof`,
  never at runtime — emits `push bp; mov bp, sp` and
  jumps straight to `mov ax, 2; ret`. We allocate
  `dec sp; dec sp` and a matching `mov sp, bp` epilogue
  for a 4-byte excess. The fix is the same "live local"
  pass deferred from the early sizeof-of-array
  probes (fixture 582 era).
- Probed `struct S { char c; }; struct S s; char b; s.c
  = 'Z'; b = s.c; return b;` as fixture 1089 first
  draft. BCC's char-assign-from-char-member skips the
  `cbw` between load and store because both sides are
  byte-width. Our codegen routes through `emit_expr_to_
  ax` which always widens, then stores AL — leaving
  a stray 1-byte `cbw` that BCC doesn't emit. Sibling
  of the char-init Member peephole already in
  `emit_init_local`; needs the same peephole on the
  *assign* path.



Fixtures `1085` (`char a = 3; char c = a << 2;` — char
left-shift init, sibling of 1082), `1086` (`unsigned
char a = 200; unsigned char c = a >> 2;` — uchar right-
shift init, exercising the promote-to-signed-int rule),
`1087` (`char a = 64; char c = a >> 4; return c;` —
char right-shift by K=4, exercising the CL form of the
shift unroll).

1087 already worked end-to-end via the batch-255 shift
arm: K=4 picks the `mov cl, 4; sar ax, cl` path
(unroll threshold K ≤ 3).

1085 and 1086 needed corrections to the batch-255
shift arm:

- **Left shift on char (1085)**: BCC keeps the
  arithmetic at byte width because the high bits fall
  off either way. Emit `shl al, 1` repeated K times (or
  `mov cl, K; shl al, cl` for K ≥ 4). No widen
  needed. Our previous code always widened to int and
  used `shl ax, 1`, which would have been one byte
  longer because the AX form takes the same opcode but
  the operand resolution differs (`d1 e0` vs `d0 e0`?).
  Actually it's one byte: `shl al, 1` is `d0 e0` (2
  bytes) vs `shl ax, 1` is `d1 e0` (2 bytes) — same
  size. The diff was elsewhere; reading BCC's pattern
  shows BCC ALWAYS uses the AL form for `<<`, which
  saves the `cbw` (1 byte) we were emitting.
- **Right shift on uchar (1086)**: BCC always uses
  `sar` regardless of the operand's declared
  signedness, because C promotion converts both `char`
  and `uchar` to *signed* `int` before the shift. Our
  previous code branched on `is_unsigned` and emitted
  `shr` for uchar, diverging from BCC. Also the widen
  for uchar uses `mov ah, 0` (3 bytes) rather than the
  `xor ah, ah` (2 bytes) we were emitting. BCC
  consistently prefers the longer `mov ah, 0` form.

Updated the shift arm: split on op direction (Shl =
byte-arith AL only; Shr = widen then signed `sar`),
and use `mov ah, 0` instead of `xor ah, ah` for the
uchar widen.



Fixtures `1082` (`char a = 16; char c = a >> 1; return
c;` — char init from a shift on a char local, exercising
the C-standard promote-shift-truncate lowering), `1083`
(`char c = 'A'; int n = c + 1; return n;` — int init
from a char-plus-const expression, requiring the
char-widen-to-int sequence), `1084` (`struct S { int x;
int y; }; int a = 10; int b = 20; s.x = a + b; return
s.x;` — struct field assignment with a binop on int
locals as the RHS).

1083 and 1084 already worked end-to-end. 1083 widens
the char load with `mov al, <c>; cbw; add ax, 1` then
stores AX to `n`'s int slot. 1084 evaluates `a + b`
into AX via the int-binop arm, then stores to the
struct field's `[bp+(s_off + 0)]` slot.

1082 hit the char-init panic — the binop arm only
covered `+/-/&/|/^` (byte-machinable ops). Shifts are
different: C promotes char to int before shifting, so
BCC emits `mov al, <a>; cbw; sar ax, K; mov <c>, al`
(or `shr` for unsigned, `shl` for left-shift). The
result still ends up in AL for the byte store.

Added a shift arm to the char-init peephole. It handles
constant K with the standard unroll: K ≤ 3 emits
repeated `<mnem> ax, 1` (2 bytes each); K ≥ 4 emits
`mov cl, K; <mnem> ax, cl` (4 bytes). Sign-pattern
dispatch picks `sar` for signed-char `>>`, `shr` for
unsigned-char `>>`, `shl` for `<<` regardless.



Fixtures `1079` (`char c = a | b;` — char init from char
OR), `1080` (`char c = a ^ b;` — char init from char
XOR), `1081` (`return sizeof(char);` — bare-type sizeof
of char in return position).

All three already worked end-to-end. 1079 and 1080
exercise the batch-243 byte-arith peephole's remaining
`|` and `^` mnemonics (alongside `+/-/&` already
covered by fixtures 1046/1051/1073). 1081 constant-
folds `sizeof(char)` to 1 at parse time, then the
return-int path emits `mov ax, 1`.

## Char ptr sub, char cmp zero as value, for loop accumulator

Fixtures `1076` (`char a[5]; char *p = a+1; char *q =
a+4; return q - p;` — pointer subtraction on two char
pointers, stride=1 so no divide-by-elem-size step
needed), `1077` (`char c = 0; int r = c == 0; return
r;` — char equality test against zero with the boolean
result stored into an int local), `1078` (`int i, s = 0;
for (i = 0; i < 4; i++) s = s + i;` — for-loop summing
0+1+2+3, the canonical iteration shape).

All three already worked end-to-end:

- 1076: `q - p` on char pointers emits `mov ax, <q>;
  sub ax, <p>` directly — no element-size divide
  because `sizeof(char) == 1`. The pointer-sub-sizeof
  divide path (deferred from batch 249) only kicks in
  for pointers to non-byte types, which this fixture
  avoids.
- 1077: `c == 0` lowers via the char-vs-zero compare
  peephole to `cmp byte ptr <c>, 0; je .L1; xor ax, ax;
  jmp .end; .L1: mov ax, 1; .end:`, then the int init
  stores AX. Already covered by the compare-as-value
  arm.
- 1078: standard for-loop emission with the typical
  pre-cond, body, post-update, jump-back shape. `s = s
  + i` lowers as `mov ax, [bp-Ns]; add ax, [bp-Ni];
  mov [bp-Ns], ax`. Already covered.



Fixtures `1073` (`char a = 12; char b = 10; char c = a &
b; return c;` — char init from a `&` binop on two char
locals, sibling of fixture 1046's add and 1051's sub
covering one more op in the byte-arith peephole's
permitted set), `1074` (`int x = 5; x = 10; return x;`
— int local initialized, then immediately reassigned
to a different constant), `1075` (`return -7;` — bare
return of a negative integer literal).

All three already worked end-to-end:

- 1073: the batch-243 char-binop peephole accepts `&`
  along with `+/-/^/|`, so `a & b` emits `mov al, <a>;
  and al, <b>; mov <c>, al`. Byte-arithmetic stays at
  byte width because the destination is char.
- 1074: the second assign `x = 10` is just another
  constant-store to the same stack slot; no peephole
  combines it with the init.
- 1075: `-7` constant-folds to 0xFFF9 (sign-extended
  i16), and the return-int path emits `mov ax, 65529`.
  BCC writes negative constants as their unsigned-
  wrapped form (same shape as fixture 036).



Fixtures `1070` (`int x = 0; do { x++; } while (x < 3);
return x;` — do-while loop counter, the rotating
sibling of fixture 1044's while form), `1071` (`int x =
5; return ~x;` — int local with bitwise complement
applied at return), `1072` (`int a[5]; a[0]..a[4] = ...;
return a[4];` — stack int array of size 5 with five
constant-store writes and a final-element read).

All three already worked end-to-end:

- 1070: the do-while emits the back-edge loop with the
  condition at the bottom: `<top>: inc word ptr [bp-N];
  cmp word ptr [bp-N], 3; jl <top>`. The body executes
  unconditionally on first iteration; the condition
  decides whether to back-edge.
- 1071: `~x` lowers via `emit_unary_not` to `mov ax,
  [bp-N]; not ax`. Standard arm.
- 1072: each `a[K] = imm` lowers to `mov word ptr
  [bp+(base+K*2)], imm` via the stack-array-elem const-
  store path. The size-5 array reserves 10 bytes; the
  final read of a[4] is at `[bp-2]`. Already covered by
  the standard stack-array path.



Fixtures `1067` (`return sizeof(int);` — bare-type
sizeof in return position, must fold to `2`), `1068`
(`struct S { int x; int y; char c; }; return sizeof
(struct S);` — struct-type sizeof exercising the layout
calculator and any padding it would compute), `1069`
(`long a = 100L; long b = 30L; long c = a - b; return
(int)c;` — long subtraction across two stack longs,
sibling of fixture 1037's add form).

All three already worked end-to-end:

- 1067: `sizeof(int)` constant-folds to 2 at parse
  time, so `return sizeof(int)` is identical to
  `return 2`. The return-int path emits `mov ax, 2`.
- 1068: the struct layout calculator (size+align)
  computes 6 (int + int + char rounded to 6 for
  alignment? or padded?). Whatever the value, it
  constant-folds at the sizeof site and the return
  path stores the constant in AX.
- 1069: the long-sub-with-borrow path emits `mov ax,
  [a+2]; mov dx, [a]; sub dx, [b]; sbb ax, [b+2]` (or
  similar HI/LO ordering), then stores DX:AX to c's
  stack slots. Already covered by batch 119's general
  long-arith path.



Fixtures `1064` (`char a[3]; char c; a[0] = 'X'; c =
a[0]; return c;` — char local read-assigned from a stack
char-array element, then returned), `1065` (`struct S {
int x; int y; }; s.x = 1; s.y = 2; return s.x + s.y;` —
stack struct with two field writes and field sum
return), `1066` (`int a[4]; int *p = a + 1; *p = 5;
return *p;` — stack-resident int pointer initialized
from a stack-array decay with constant offset).

1064 and 1065 already worked end-to-end:

- 1064: `c = a[0]` lowers to `mov al, byte ptr [bp-Na];
  mov byte ptr [bp-Nc], al` via the stack-array-elem
  byte-load and the assign-to-char-local path.
- 1065: struct field assigns and the field-sum read
  hit the standard `[bp+(struct_off + field_off)]`
  arithmetic. Already covered.

1066 exposed a gap. The batch-243 array+const peephole
in `emit_store_reg` covered REGISTER-resident pointer
init (`int *p = a + 1` with p in SI). The STACK-resident
case (the harness assigns p to `[bp-N]` here because of
how the locals planner distributes registers) routed
through the generic `emit_expr_to_ax` path, which emits
`lea ax, [bp+base]; inc/add ax, K; mov [bp-Np], ax` —
the inc/add is wrong (stride-unaware) and BCC instead
folds the offset into the LEA.

Added the same array+const-offset peephole to
`emit_init_local`'s int-like stack arm. Now stack
pointer inits of the shape `<arr> + K_const` emit the
single `lea ax, [bp+(base+K*stride)]; mov [bp-N], ax`
sequence.

**Recorded finding (deferred):**

- Probed `int a[4]; int *p = a+1; int *q = a+3; return
  q - p;` as fixture 1066 first draft. After the
  stack-pointer-init fix above, our code computed
  `sub ax, [bp-Np]` for the pointer diff but missed
  BCC's `mov bx, 2; cwd; idiv bx` divide-by-element-
  size sequence. The pointer-subtraction-with-pointee-
  sizeof shape is a separate codegen change (need
  to detect ptr-minus-ptr at the BinOp::Sub arm and
  apply the divide tail for non-byte pointees).
- Probed `int *p, *q` with both writing through SI/DI;
  hit a missing tasm encoding `mov word ptr [di], imm`
  (we only support SI). Sibling `MovDiPtrImm` IR
  variant needed.



Fixtures `1061` (`int x = 3; return 10 - x;` —
subtraction with constant LHS and variable RHS, the
opposite operand order from the more common `x - K`
shape), `1062` (`int a[3]; int *p = &a[1]; *p = 100;
return a[1];` — int pointer to a specific array
element, dereference-write through the pointer, then
read the same element back), `1063` (`char a = 5;
char b = 3; if (a > b) return 1; else return 2;` —
char-vs-char compare in an if-else condition with
two byte-register-resident operands).

All three already worked end-to-end. 1061's `10 - x`
lowers as `mov ax, 10; sub ax, [bp-N]` via the
constant-LHS arm; 1062 routes the address-of-element
through the batch-243 stack-array LEA peephole and the
deref-write through `mov word ptr [si], 100`; 1063
uses the char-vs-char compare peephole with `jbe` as
the inverse jump for the if-else dispatch.



Fixtures `1058` (`char c = 5; c--; return c;` —
sibling of fixture 1056 with `--` instead of `++`, used
to confirm the byte-register stmt-position split also
covers dec), `1059` (`int x = 0; int *p = &x; *p = 7;
return x;` — int pointer to a stack local, dereference-
write through the pointer, then read the local back),
`1060` (`int x = 5; if (x > 0) return 1; else return
2;` — if-else with each branch being a bare `return`).

All three already worked end-to-end:

- 1058: the batch-246 byte-register stmt arm dispatches
  on the mnemonic (`inc` for `++`, `dec` for `--`) and
  emits `dec <reg>` directly when the position is Post.
- 1059: `&x` for a stack local lowers to `lea ax, [bp-
  N]; mov si, ax` (or similar register), then `*p = 7`
  is a memory-direct `mov word ptr [si], 7` store. Read
  of `x` afterwards picks up the new value via its
  stack slot.
- 1060: the if-else codegen emits `cmp; jle .L1; mov ax,
  1; jmp .end; .L1: mov ax, 2; .end:` then the function
  epilogue. Each branch's `return` is a terminator that
  doesn't get its own jump-to-end since the else
  already takes over from the if's fallthrough.



Fixtures `1055` (`int a = 10; int b = 3; int c = 2;
return a - b - c;` — three-way left-associative
subtraction across three stack locals, sibling of
fixture 1032's add form), `1056` (`char c = 'A'; c++;
return c;` — char postincrement positioned as a stmt
on its own line, value discarded), `1057` (`int x;
return (x = 5, x + 1);` — int returned from a comma
expression with an assignment side-effect).

1055 and 1057 already worked end-to-end. 1057's comma
expression evaluates `x = 5` for its side effect, then
the comma value is `x + 1`, which is what gets returned
— routed through the standard return-int path.

1056 emitted 4 extra bytes — our `emit_update_in_place`
arm for byte-register locals always stages through AL
(`mov al, <reg>; inc al; mov <reg>, al`), but BCC only
uses that for *pre*-increment. For *post*-increment as
a discarded stmt, BCC emits the direct `inc <reg>` form
(2 bytes).

Pre vs post matters even when the value is discarded
because BCC's frontend lowers them through different
paths. Updated the byte-register arm to split: pre keeps
the AL detour (fixtures 047/050–054/123–125/148/156 all
exercise this); post emits `inc <reg>` / `dec <reg>`
directly.



Fixtures `1052` (`int a[4]; int *p = a + 2; a[2] = 55;
return *p;` — sibling of fixture 1047 with K=2 instead
of K=1; exercises the batch-243 array+const-offset
LEA peephole with a different stride product), `1053`
(`int a[3]; int v = 42; a[1] = v; return a[1];` —
stack-array element assigned from an int local (variable
RHS) rather than a constant), `1054` (`int x = 10; x =
x + 5; return x;` — int init followed by a "rebind
to self plus const" reassignment).

All three already worked end-to-end. 1052 exercised the
LEA peephole's offset math at K=2 (adj_off = base + 4
bytes); 1053 went through the existing stack-array
elem variable-RHS write path; 1054 has the assign
arm with the constant-add peephole.

**Recorded finding (public-symbol ordering — partial map):**

Probed the symbol-ordering rule by running the oracle on
`int <name>(void) { return 42; } int main(void) { int n
= <name>(); return n; }` for many `<name>` choices. The
PUBDEF order in the OBJ depends on the function name in
ways not yet reduced to one rule, but the data points
catalog:

| name        | order        |
|-------------|--------------|
| f, a, b, c, d, e, g, h, i, k, l, m, z | main first |
| aa          | main first   |
| mm, ma, mae, mai, mainn? | main first (mainn is *name* first) |
| main2       | main2 first  |
| ff, fff, ffff, fffff, fb, fff | name first |
| zz, abc, xyz | name first  |
| helo, helper, helper2 | name first |
| gimme, my_fn, mymain, mais, maib | name first |
| _f          | _f first     |
| _gimme      | main first   |

Forward-declaring main *before* the helper in source order
doesn't change the ordering for single-char or m-prefix
names but does flip a few (e.g., `aa` and `f` then put
main first regardless).

Not alphabetical, not by length, not by source position.
The pattern is consistent with a hash-table-bucket
walk — the symbol's hash determines its position. We
don't yet know the hash function or bucket count. Until
pinned, any multi-function probe whose helper name
falls in the "wrong" bucket will diverge.

Going forward: avoid multi-function fixtures except where
the helper name is single-character (`f`-class), or use
forward-declared main + body-after for predictable
ordering when needed.



Fixtures `1049` (`int a[3]; int *p = &a[1]; a[1] = 99;
return *p;` — explicit address-of-element form of the
batch-243 `a + 1` shape), `1050` (`char c = 'A'; return
(int)c;` — explicit `(int)` cast in return position),
`1051` (`char a = 10; char b = 3; char c = a - b;
return c;` — sibling of fixture 1046 exercising the
`sub al, byte ptr <b>` byte-arith path).

All three already worked end-to-end:

- 1049: the AST shape for `&a[1]` is
  `AddressOf(ArrayIndex(Ident("a"), IntLit(1)))`, which
  routes through the array-element address path
  (`emit_array_addr_to_bx` / `try_lvalue_chain_addr`)
  and produces the same `lea ax, [bp+(base+K*stride)]`
  computation as the batch-243 `a + 1` peephole. Both
  forms emit the byte-identical address-load — the
  parser distinguishes the two syntactic shapes but
  codegen converges on one folded LEA.
- 1050: `(int)c` in return position is the standard
  char-load-and-widen sequence: `mov al, byte ptr <c>;
  cbw`. The explicit cast is parsed but doesn't change
  codegen — the return-int arm already widens char-like
  return values via cbw.
- 1051: the batch-243 char-binop peephole accepts any
  op in `{+, -, &, |, ^}`. `sub` was added alongside
  `add`/`and`/`or`/`xor` so this fixture goes through
  the same `mov al, <a>; sub al, <b>; mov <c>, al`
  shape with no new code.

**Recorded finding (deferred):**

- Probed `int gimme(void) { return 42; } int main(void) {
  int n = gimme(); return n; }` as fixture 1050 first
  draft. The OBJ differed by 1 byte at offset 160 (the
  PUBDEF block): BCC emits `_gimme, _main` while we emit
  `_main, _gimme`. Same public-symbol ordering rule we
  haven't pinned (batches 218/236). Replaced with the
  no-call char-cast shape until the ordering heuristic
  is identified.



Fixtures `1046` (`char a = 5; char b = 3; char c = a + b;
return c;` — char init from a binary op on two char
locals, byte-level arith without int promotion), `1047`
(`int a[3]; int *p = a + 1; a[1] = 42; return *p;` —
register-resident int pointer initialized from a
stack-array decay + constant offset), `1048` (`struct S
{ int x; int y; }; s.x = 42; s.y = 17; return s.x;` —
struct-field assignment with two field writes and a
field read for return).

1048 already worked end-to-end — struct-field assigns
and reads through the standard `bp_addr` arithmetic
have been wired since the very first struct fixtures.

1046 hit the batch-241 panic — char init from a binop
RHS. BCC keeps the arithmetic at byte width when the
destination is char: `mov al, byte ptr <a>; add al,
byte ptr <b>; mov byte ptr <c>, al`. No int promotion
because the result is truncated anyway.

Added a char-init binop arm: when both operands are
stack-resident char locals and the op is one of
`+/-/&/|/^` (the byte-machinable group; `<<`, `>>`,
`*`, `/`, `%` lack 8-bit reg-vs-mem forms on 8086),
emit the three-instruction byte-arith sequence
directly. Other char-init binop shapes (mixed types,
non-stack operands) still panic until pinned.

1047 emitted a buggy `lea ax, [bp-6]; inc ax; mov si,
ax` — the `+1` was added as a literal byte rather than
scaled by `sizeof(int)`. The `inc ax` would have left
SI pointing at the high byte of `a[0]`, not at `a[1]`
as the C source intends. BCC's pattern folds the
element offset into the LEA: `lea ax, [bp-4]; mov si,
ax` (because `&a[1]` = `&a[0] + 2 = [bp-6+2] = [bp-4]`).

Added a register-init peephole in `emit_store_reg`: when
the RHS is `<stack-array-ident> + K_const`, compute
`base + K * elem_size` at compile time and emit one
`lea ax, [bp+adj_off]; mov <reg>, ax` pair. Removes both
the stride bug and the extra `inc/add` instruction.



Fixtures `1043` (`char c = 'A'; int n = c; return n;` —
int local initialized from a char local, implicit
widening that needs `mov al, byte ptr <src>; cbw; mov
word ptr <dst>, ax`), `1044` (`int x = 0; while (x < 3)
x++; return x;` — minimal while-loop with a single-stmt
body, no braces), `1045` (`int n = 65; char c = n;
return c;` — char init from an int local without an
explicit cast, the implicit-narrowing sibling of fixture
1039).

All three already worked end-to-end:

- 1043: the int-init arm of `emit_init_local` routes
  through `emit_expr_to_ax`, which for an `Ident("c")`
  whose type is char-like loads `mov al, byte ptr <src>;
  cbw` and then the init stores `mov word ptr <dst>, ax`.
  The char-widen-to-int sequence has been wired since
  the very first char fixtures.
- 1044: `while (x < 3) x++;` parses as a `WhileStmt` with
  a single expression-statement body. The codegen
  emits the standard back-edge loop: `<top>: cmp word
  ptr [bp-N], 3; jge <end>; inc word ptr [bp-N]; jmp
  <top>; <end>:`. Already covered by the early while-loop
  fixtures.
- 1045: thanks to batch 241's generalization, char init
  from a bare-ident source (whether char or int local)
  routes through the same byte-load peephole. `char c =
  n;` with n int emits `mov al, byte ptr <n>; mov byte
  ptr <c>, al` — the implicit narrowing is just "use the
  low byte". Same byte sequence as the explicit-cast
  form (fixture 1039).



Fixtures `1040` (`char a = 'A'; char b = a; return b;` —
char local initialized directly from another char local,
the simplest "byte-to-byte copy" shape), `1041` (`int x =
17; return x % 5;` — int modulo by a constant divisor in
return position), `1042` (`int x = (1 + 2) * 3; return
x;` — int init from a fully-constant compound expression
that should fold to 9 at parse time).

1041 and 1042 already worked end-to-end:

- 1041: `x % 5` materializes the divisor in BX
  (`mov bx, 5`), sign-extends AX into DX:AX with `cwd`,
  then `idiv bx` — modulo result is in DX which is then
  moved to AX for the return. The BX-load form was added
  in slice 200's `idiv <bx>` arm for compound `%=` and
  re-used here for the standalone `%` expression.
- 1042: `try_const_eval` folds `(1 + 2) * 3` to `9` at
  the init-evaluation step, then the stack-init's
  constant arm emits `mov word ptr [bp-N], 9`. No
  expression evaluation reaches codegen. Already
  covered.

1040 hit the new panic that batch 240 added — `non-
constant char local init shape not yet supported`. The
init expression is a bare `Ident("a")` rather than a
`Cast` of one, so the cast-unwrap arm didn't apply. BCC
emits the same `mov al, byte ptr [bp-Na]; mov byte ptr
[bp-Nb], al` sequence whether or not the source was
cast — the byte load doesn't care about the source's
declared width since it always reads a single byte
from `[bp+off]`.

Generalized the peephole: optionally peel an outer
`(char)` cast off the init, then accept any stack-local
source whose declared type is char-like or int-like.
Both the cast form (fixture 1039) and the bare-ident
form (fixture 1040) now route through the same emit.
Non-stack and non-ident char init RHS shapes still
panic until pinned.



Fixtures `1037` (`long a = 5L; long b = 10L; long c = a +
b; return (int)c;` — long+long stack-local arithmetic
materialized into a third stack-local, then truncating
cast back to int for the return), `1038` (`int x = a ?
b : c;` — ternary expression directly initializing a
stack int local), `1039` (`int n = 65; char c = (char)n;
return c;` — non-constant char init from an explicit cast
of an int local).

1037 and 1038 already worked end-to-end:

- 1037: the long-arith arm in `emit_init_local`'s
  `long_like` branch covers `long c = a + b` through
  `try_emit_long_value_to_dest`, which loads both
  operands into DX:AX, adds with carry, and stores to
  the destination's HI/LO slots. The `(int)c` cast just
  loads the low word of `c` into AX. Already covered
  by the batch-119 long-arith dest-mem path (fixture
  357 was the canonical probe).
- 1038: ternary in an init position lowers to a
  `branch on cond / mov ax, then / jmp end / lab: mov
  ax, else / end:` sequence routed through
  `emit_expr_to_ax`. The init-local arm then stores AX
  to the stack slot. The condition `a` (int local) is
  a non-zero test (`cmp word ptr [bp-N], 0; je <else>`),
  the same shape as `if (a)` from much earlier. No
  ternary-init-specific code needed — the general
  ternary-as-expression path already wrote AX.

1039 panicked at the assert in `emit_init_local`'s
char-init fallback: `non-constant init for non-int-like
type Char not yet supported`. BCC's expected shape is
the tightest possible — load the LOW byte of the int's
slot directly with `mov al, byte ptr [bp-Nn]` (since the
int and its low byte share the same address in the
small-endian frame), then store with `mov byte ptr [bp-
Nc], al`. No widen/truncate round-trip through AX.

Added a peephole arm: when the char init's RHS is
`Cast { ty: Char, operand: Ident(src) }` and `src` is a
stack int local, emit the two-byte `mov al, byte ptr ...
; mov byte ptr ..., al` sequence directly. Other char-
init RHS shapes still panic until pinned (no fixture
yet).



Fixtures `1034` (`int a = 10; int b = 3; return a - b;` —
subtraction of two stack-resident int locals as the
function's return value), `1035` (`int n = 5; if (n > 0)
n++; return n;` — postincrement on a stack-local
positioned inside a single-statement if-body, no braces),
`1036` (`int a = 0x10; char b = 0x02; return a | b;` —
int local OR'd with a char local; the char promotes to int
via the standard `mov al, [bp-N]; cbw` widen sequence
before the OR).

All three already worked end-to-end:

- 1034: `a - b` loads `a` into AX, then `sub ax, word ptr
  [bp-Nb]` — same memory-direct binop arm used for `+`
  and bitwise ops. Already covered.
- 1035: `if (n > 0) n++;` parses the if-body as a single
  expression-statement. The codegen emits `cmp word ptr
  [bp-N], 0; jle <skip>; inc word ptr [bp-N]; <skip>:`.
  The single-statement if-body already worked since the
  if-stmt arm accepts any statement, not just blocks.
  The postinc-as-stmt path uses `IncBpRel` directly when
  the value isn't consumed.
- 1036: the `|` arm sees a non-char LHS (int) and a char
  RHS. The RHS evaluation goes through `emit_expr_to_ax`
  which widens char-to-int via `cbw`. The OR then operates
  on AX with the int-LHS source. Already covered.



Fixtures `1031` (`int x = 5; if (x != 7) return 1; return
0;` — int local compared with `!=` against a non-zero
constant; the not-equal branch picks `je` as the "fall-
through" jump), `1032` (`int a = 1; int b = 2; int c = 3;
return a + b + c;` — three-way left-associative addition
across three stack locals), `1033` (`int x = 5; int y =
-x; return y;` — unary negation of a stack-local
materialized into AX, then stored back to a second stack
local).

All three already worked end-to-end:

- 1031: `!=` in if-condition lowers via the existing
  compare-then-jump arm with `je <skip>` as the inverse-
  branch dispatch. RHS is `7` (imm8sx), encoded via
  `cmp word ptr [bp-N], 7` (`83 7E dd 07`, 4 bytes).
  Already covered alongside `==` (its sibling), which we
  captured back when `if (x == K)` first landed.
- 1032: `a + b + c` parses left-associatively as `(a +
  b) + c`. The codegen evaluates `a + b` into AX (load a,
  add b), then adds c directly: `mov ax, [bp-N1]; add
  ax, [bp-N2]; add ax, [bp-N3]`. No push/pop pair needed
  since the LHS of the outer `+` already lives in AX
  when the RHS is a memory-direct load. The mem-direct
  binop arm was added back during the early arith
  batches.
- 1033: `-x` lowers via `emit_unary_neg`: load `x` into
  AX, then `neg ax`. The init `int y = -x` stores AX to
  `y`'s slot via the standard assign-local path. Already
  covered (batch 110's sibling probe noted in passing
  during fixture 650's free-pass).



Fixtures `1028` (`unsigned int x = 100; if (x > 5) return 1;
return 0;` — unsigned-typed local compared against an
imm8, must use the unsigned jump form `jbe` rather than
`jle`), `1029` (`int x = 1; x++; x++; return x;` — two
back-to-back postincrements on the same stack-local with
no intervening uses), `1030` (`int x = 128; int r = x >>
4; return r;` — int shr by a constant K ≥ 4, materializes
through `mov cl, K; sar reg, cl`).

All three already worked end-to-end:

- 1028: the `unsigned` storage-class flows to `expr_is_unsigned`
  on the lhs of the compare; the compare arm picks `jbe` for
  the "not greater-than" jump (unsigned form). RHS is imm8sx
  (`5`) so the encoding is the existing `cmp word ptr [bp-N],
  imm8sx` (`83 7E dd ii`, 4 bytes). No new shape needed —
  the unsigned-jump dispatch was added back during the very
  first unsigned-int batches.
- 1029: each `x++` lowers to `inc word ptr [bp-N]` (3 bytes
  via the existing `IncBpRel`/`DecBpRel` direct-memory IR
  variants). The two increments are independent stmts with
  no shared CSE, and BCC also emits the pair back-to-back —
  no temporal coalescing. Already covered.
- 1030: K=4 is above the K ≤ 3 unroll threshold, so the
  shift arm picks the CL form: `mov ax, [bp-N]; mov cl, 4;
  sar ax, cl`. The init `int r = x >> 4` then stores AX to
  `r`'s stack slot. Already covered; `sar` is the signed-int
  shift dispatch (batch 232's split keeps `>>` on signed-int
  operands routed to `sar`).

**Recorded finding (baseline failure count correction):**

- Full regression now shows **12** pre-existing failures
  rather than the previously recorded 11. Fixture
  `586-char-add-char-obj` (`char a; char b; a=1; b=2;
  return a+b;`) has been failing back to its capture in
  commit 999b0ae — bisecting through every codegen
  commit in the session shows the same 236-byte output,
  while the oracle captured 238 bytes. The 2-byte
  difference is in the operand-eval order for char-promoted
  addition: BCC evaluates the LHS first, pushes it,
  evaluates the RHS, then does `mov dx, ax; pop ax; add
  ax, dx` (4 bytes of save/restore). We instead evaluate
  the RHS first, push, evaluate the LHS, then `pop dx;
  add ax, dx` (2 bytes of save/restore — we're tighter
  than BCC by 2 bytes). The byte-exact invariant means
  this counts as a divergence to fix even though we're
  smaller. Deferred — needs an operand-order rule for
  char-promoted commutative adds (LHS first, RHS second,
  with `mov dx, ax; pop ax` rather than `pop dx`).

## Null-ptr cast init, two stack arrays sum, int le-cmp as value

Fixtures `1025` (`int *p = (int *)0; if (p == 0) return 1;` —
local pointer initialized from a casted integer-zero, then
compared to zero), `1026` (`int a[2]; int b[2]; a[0]=5;
a[1]=10; b[0]=1; b[1]=2; return a[0] + b[0];` — two adjacent
stack-array slots written and one elem from each summed),
`1027` (`int x = 3; int y = 5; int r = x <= y; return r;` —
relational `<=` materialized into an int local rather than
consumed by a branch).

All three already worked end-to-end:

- 1025: `(int *)0` constant-folds to a null pointer; the
  init lowers to `mov word ptr [bp-2], 0`. The `if (p ==
  0)` then uses the existing stack-local-vs-zero zero-test
  arm (`cmp word ptr [bp-2], 0; jne <skip>`) added in
  batch 221's sibling — no new shape needed.
- 1026: each `a[i] = K` / `b[i] = K` resolves to a
  `mov word ptr [bp-N], imm16` store via the stack-array-
  elem assign path. The final `a[0] + b[0]` loads one slot
  into AX and adds the other directly (`add ax, word ptr
  [bp-M]`). All paths already existed from batches 220/222.
- 1027: `x <= y` in rvalue position lowers via the existing
  compare-as-value path: `mov ax, [bp-N]; cmp ax, [bp-M];
  jg .L1; mov ax, 1; jmp .L2; .L1: xor ax, ax; .L2:`. The
  result lands in AX and the assign-local path stores it
  to `r`. No new branch-cond shapes — `<=` uses `jg` as
  the "not-le" jump just like the if-stmt path. The
  batch-232 shift-vs-compare signedness split kept the
  signed jump for signed int operands.

**Recorded finding (deferred):**

- **Public-symbol ordering for int-returning helper +
  caller**: probed `int gimme(void) { return 'A'; } int
  main(void) { return gimme(); }` as fixture 1027 first
  draft. Public-symbol order in our PUBDEF was `_main,
  _gimme` while BCC emits `_gimme, _main`. Same unidentified
  ordering heuristic as the earlier `dbl/g/main` probe
  (batch 218 series). The "all-int-typed" helper case
  doesn't disambiguate cleanly against fixture 138's
  `_main, _f` shape. Probe replaced with the int-le-cmp
  shape until we have appetite for more probes targeting
  the ordering rule.

## int `<<=` unroll for K≤3, char init expr, int `*=` pow2

Fixtures `1022` (`int x = 3; x <<= 2;` — int compound shift
by constant, must unroll rather than use CL), `1023`
(`char c = 'A' + 1;` — char initialized from a constant
expression), `1024` (`int x = 3; x *= 4;` — int compound
multiply by power-of-2 constant).

1023 and 1024 already worked end-to-end. The `'A' + 1`
expression is constant-folded at parse time to `66`; the
char init lowers identically to fixture 011 (`char c = 1`).
1024's `x *= 4` unrolls to two `shl si, 1` via the existing
power-of-2 multiplication peephole.

1022 exposed a missed unroll. The compound-shift-on-int-
register arm (around line 5200) was always emitting the CL
load (`mov cl, K; shl reg, cl`) regardless of K's
magnitude. BCC actually unrolls for K = 1, 2, 3 into
repeated `<mnem> <reg>, 1` (2 bytes each) and uses the CL
form for K ≥ 4 (5 bytes). Same threshold as the expression-
context shift (fixture 626) — the existing `Shl`/`Shr`
arm in `emit_op_with_source` already does the unroll.
Updated the compound-shift arm to match: when K ∈ {1, 2,
3}, emit `<mnem> <reg>, 1` repeated K times; otherwise use
the CL form. Saves 1 byte for K=2 (4 vs 5) and matches BCC
byte-for-byte.

Note this only affects compound shifts on register-resident
int locals. The expression-position shift already unrolled
correctly via `emit_op_with_source`; this batch closed the
compound-shift arm gap.

## char-ptr subscript read, parens-add cmp, int mul then add

Fixtures `1019` (`char *p; return p[1];` — char-pointer
subscript read through SI), `1020` (`if ((a + b) > 5)` —
explicit-parens-add in if condition), `1021` (`int r = a *
b; return r + 1;` — mul stored to a local, then add to a
const).

1019 needed the sibling of batch 233's byte-store IR. Added
`MovReg8ByteSiDisp { reg, disp }` for `mov reg8, byte ptr
[si+disp]`:
- disp=0: `8A (00_reg_100)` = 2 bytes
- disp!=0 fitting i8: `8A (01_reg_100) dd` = 3 bytes
Parser matches `mov reg8, byte ptr [si+disp]` via the new
`parse_byte_si_disp` helper (added in batch 233 for the
sibling store).

1020 already worked. The `(a + b) > 5` lowers as `mov ax,
[bp-N]; add ax, [bp-M]; cmp ax, 5` — the parentheses are
parsed but don't affect codegen since `+` and `>` already
have the right precedence relationship.

1021 already worked end-to-end. `r = a * b; return r + 1;`
emits `mov ax, [bp-N]; imul [bp-M]; mov [r], ax; ...
mov ax, [r]; add ax, 1`. Each statement is independent; no
op-ordering peephole needed since the mul result is staged
through a stack slot.

**Recorded finding (deferred):**

- **Operand-reorder for commutative ops mixing complex and
  simple operands**: probed `return a * (b + c);` and got
  a 4-byte difference. Our codegen evaluates `(b + c)`
  into AX first, then pushes it, then loads `a` into AX,
  then pops to DX and `imul dx`. BCC instead evaluates
  `(b + c)` into AX first, then uses `imul word ptr <a>`
  directly against the memory operand — no push/pop
  round-trip. The optimization is to recognize when a
  binop's "complex" side has already produced AX and the
  "simple" side is mem-direct, then use the memory-form
  of the second op rather than swapping through DX. Sibling
  of existing memory-direct binop arms but applied to the
  commutative-swap case.

## char-ptr subscript byte store, int ptr subscript write, int cmp imm16

Fixtures `1016` (`char a[3]; char *p = a; p[1] = 'B';` —
char-pointer subscript write needs a byte memory-direct
store through an SI-resident pointer), `1017` (`int a[3];
int *p = a; p[1] = 99;` — int-pointer subscript write,
already covered word-store path), `1018` (`x = 1000; if (x
== 1000)` — int local cmp imm16, exercises the wide-
immediate form of `cmp word ptr [bp-N], imm`).

1016 needed a new tasm IR variant. `MovByteSiDispImm8 {
disp, imm }` encodes `mov byte ptr [si+disp], imm8`:
- disp=0: `C6 04 ii` (3 bytes, ModR/M mod=00 r/m=100)
- disp!=0 fitting i8: `C6 44 dd ii` (4 bytes, mod=01)
Sibling of the existing `MovBpRelImm8` (bp-relative byte
store). Parser accepts `byte ptr [si+disp]` LHS with imm8
RHS via the new `parse_byte_si_disp` helper.

1017 already worked end-to-end — the int-pointer subscript
write went through the existing word-store-through-SI path
(`MovSiPtrImm`, fixture 136's sibling). No char-specific
shape needed since int stores already had the byte-vs-word
distinction baked in.

1018 already worked. `cmp word ptr [bp-N], 1000` uses the
imm16 form of Group-1 CMP (`81 7E dd lo hi`, 6 bytes) since
1000 doesn't fit imm8sx (-128..127). The existing
`CmpBpRelImm16` IR variant (fixture 563) handled this.

## Array-elem cmp self, uchar shr var, uchar shr const

Fixtures `1013` (`if (a[0] == a[1])` — two stack-array
elements compared to each other), `1014` (`uchar c; int n;
return c >> n;` — uchar shifted by a variable count), `1015`
(`uchar c = 128; return c >> 2;` — uchar shifted by a
constant).

1013 already worked end-to-end via the batch-220 rvalue
ArrayIndex fallthrough — both operands resolve to `[bp+N]`
operand sources, the compare emits `mov ax, [bp+N1]; cmp
ax, [bp+N2]` then dispatches the signed jump.

1014 and 1015 exposed a missed signedness rule. C's integer
promotion converts char/uchar to *signed* int (because int
can hold all char values), and the `>>` mnemonic should
follow the promoted type — `sar` (arithmetic shift right)
for signed int, not `shr` (logical shift right). Our
codegen was carrying the operand's declared `unsigned`-ness
through to the shift dispatch, so uchar got `shr` while
BCC emits `sar`.

Fix is a new helper `expr_shift_is_unsigned`: same as
`expr_is_unsigned` but flattens char-like types to "not
unsigned" (since they promote to signed int). The shift-
dispatch site in `emit_expr_to_ax`'s BinOp path uses this
variant for `Shr` only — comparisons keep using
`expr_is_unsigned` because BCC actually departs from strict
C90 promotion semantics there: uchar compares pick *unsigned*
jumps (`jbe`/`jae`), not signed (fixture 459). Two distinct
"unsigned" interpretations:

|             | Shift (`>>`)         | Compare (`<`,`>=`, etc.) |
|-------------|---------------------|--------------------------|
| `int`       | sar (signed)        | jl/jge (signed)          |
| `unsigned`  | shr (logical)       | jb/jae (unsigned)        |
| `char`      | sar (signed)        | jl/jge (signed)          |
| `uchar`     | sar (signed)        | jb/jae (unsigned)        |

The shift column follows strict C promotion; the compare
column follows BCC's choice of preserving the operand's
unsignedness past the promotion. This was caught by 1015
breaking the pre-existing 459 fixture during initial fix
attempt — split the helpers to keep both byte-exact.

## char + int const, int cmp -1, int mul -3

Fixtures `1010` (`char c = 1; return c + 100;` — char + int
constant in return), `1011` (`int x = -1; if (x == -1)` —
int compared to negative literal), `1012` (`int x = 5;
return x * -3;` — int times a negative non-power-of-2
constant).

All three already work end-to-end:

- 1010: char widens via cbw, then `add ax, 100` against the
  int-sized constant. Sibling of fixture 607 (`return c +
  1`) — same widen-then-add shape; the integer constant
  fits imm8sx.
- 1011: existing `<int-local, const>` cmp arm emits `cmp
  word ptr [bp-2], -1` (3 bytes via imm8sx, `83 7E FE FF`).
  The negative literal is sign-extended at the assembly
  level; the OBJ encoder picks `-1` as `FF` byte.
- 1012: `x * -3` materializes -3 in DX (`mov dx, -3` →
  imm16 form since -3 doesn't fit imm8 for `mov r16,
  imm`), then `imul dx`. Negative constants don't trip the
  power-of-2 unrolling path (which checks `k > 0`), so
  they uniformly take the DX-load shape.

**Recorded finding (deferred):**

- **Non-static stack array initializer** (`int a[3] =
  {10, 20, 30};`): codegen panics "non-constant init for
  non-int-like type". BCC's actual lowering is interesting
  — it emits the init data as raw `db` bytes in `_DATA`,
  then calls `N_SCOPY@` to copy 6 bytes from DGROUP onto
  the stack slot:
    push ss
    lea ax, [bp-6]
    push ax
    push ds
    mov ax, offset DGROUP:d@w+0
    push ax
    mov cx, 6
    call near ptr N_SCOPY@
  Implementing this needs the codegen path for non-static
  array locals with init to (a) append the literal bytes to
  the data table, (b) emit the 7-instruction copy
  preamble, and (c) wire the helper-symbol registration
  (`N_SCOPY@`) for emit-time fixup. Same helper used by
  the struct-copy path (fixtures 416/418); the difference
  is that the source comes from a *literal* DGROUP segment
  rather than a named global.

## Arrow field cmp const (peephole), array elem cmp, ternary in return

Fixtures `1007` (`if (p->x == 5)` with p in SI — addresses
the deferred batch-228 finding), `1008` (`if (a[1] == x)` —
stack array elem compared to a local in an if condition),
`1009` (`return x > 0 ? 100 : 200;` — ternary expression in
return position).

1007 needed both a tasm IR addition and a codegen peephole:

- Added `CmpWordSiDispImm8Sx { disp, imm }` to tasm IR.
  Encoding: `83 3C ii` for disp=0 (mod=00, 3 bytes); `83
  7C dd ii` for disp!=0 fitting i8 (mod=01, 4 bytes).
  Both forms use Group1 opcode `83` with /7=CMP and SI-
  indirect r/m (r/m=100). Parser recognizes `cmp word
  ptr [si+disp], imm` via the existing `parse_word_si_disp`
  helper plus `parse_imm8_signed` for the RHS constant.
- Added a fast-path arm in `emit_compare`: when LHS is
  `Member { kind: Arrow }` whose base is a SI-resident
  pointer local and whose field is non-char and the RHS
  is a constant that fits imm8sx, emit `cmp word ptr
  [si+field_off], K` directly. Restricted to SI for now
  since tasm only has the SI form; a DI sibling would
  follow the same pattern.

Saves 1 byte vs the previous `mov ax, [si]; cmp ax, K`
shape (4 bytes vs 5).

1008 already worked end-to-end. The compare-as-value path
materialized the LHS array element through the batch-220
operand-source rvalue then ran the standard `mov ax,
[bp+elem]; cmp ax, [bp+x]` shape. The memory-direct-cmp
peephole (batch 220) only fires for constant RHS — here
the RHS is a stack local, so the generic path applies.

1009 already worked end-to-end. The ternary lowering
materializes the boolean into the standard mini-CFG with
two `mov ax, K` materializations of the constants 100 and
200 — `cmp [bp-2], 0; jle .else; mov ax, 100; jmp .end;
.else: mov ax, 200; .end: <return>`. Fixture 428/431
covered the assign-to-global and nested-ternary variants;
this confirms the return-position form is byte-equivalent.

## Enum as array size, array elem cmp local, char return in arith

Fixtures `1004` (`enum { N = 4 }; int a[N];` — enum constant
used as an array size in a global decl), `1005` (`if (a[1]
== x)` — stack-array element compared to a local variable),
`1006` (`char f(void) { return 'A'; } return f() + 1;` — a
char-returning function call used as an arithmetic operand).

1004 needed a parser extension. The global-decl array-size
grammar only accepted `IntLit` tokens — enum constants
(stored in `self.enum_constants`) were rejected with
"expected array size (integer literal), got identifier".
Extended the size-token match to also accept `Ident`,
looking up the name in the enum-constant table. The error
message also gained "(integer literal or enum constant)" to
reflect both forms. Same fix is still needed at the other 4
array-size sites (typedef'd array types, struct fields,
local declarations) — only the global-decl site is fixture-
covered today.

1005 already worked end-to-end. The compare-as-value path
materialized the LHS array element through the batch-220
operand-source rvalue and then ran the standard
`mov ax, [bp+elem_off]; cmp ax, [bp+x_off]` shape. The
memory-direct compare peephole (batch 220) only fires for
constant RHS — here the RHS is a stack local, so the
generic path applies.

1006 already worked end-to-end. `char f()` returns its
value in AL only; the caller calls `cbw` to widen AL→AX
(signed-char) or `mov ah, 0` (uchar) before the
arithmetic. Fixture 562/607 covered the widening side; this
confirms the widened AX feeds straight into a subsequent
`+ 1` via the standard `add ax, 1` op.

## Char stack array elem compound, postinc, arrow var-RHS

Fixtures `1001` (`char a[3]; a[1] += 5;` — char stack array
element compound add with const), `1002` (`char a[3];
a[1]++;` — char stack array element postinc as statement),
`1003` (`struct S { int x; } a; struct S *p = &a; int v =
42; p->x = v;` — arrow field assigned from a non-constant
stack local).

All three already work end-to-end via the existing array-
compound / array-postinc / arrow-assign paths. The char
array compound add lowers to `add byte ptr [bp+(base+K)],
imm` (same encoding as fixture 720's compound-and). The
char array postinc is `inc byte ptr [bp+(base+K)]`,
sibling of fixture 547 (int) and 717 (char global).
The arrow field var-RHS routes through `emit_member_assign`
— the batch-224 non-const arm covers both global-struct
and arrow-pointed-struct fields uniformly (the destination
operand differs but the same `mov ax, <rhs>; mov <dest>,
ax` lowering applies).

**Recorded findings (deferred):**

- **Enum constant as array size** (`enum { N = 4 }; int
  a[N];`): parser fails "expected array size (integer
  literal), got identifier". The array-size grammar only
  accepts `IntLit`; needs to fold enum constants (already
  registered in `enum_constants`) and possibly typedef'd
  integer constants too.
- **Memory-direct cmp for arrow field** (`if (p->x == 5)`
  with p in SI): BCC emits `cmp word ptr [si], 5` (4 bytes,
  imm8sx Group-1 form) — our codegen does `mov ax, [si];
  cmp ax, 5` (5 bytes). The peephole exists in spirit (see
  fixtures 891/1002 sibling probes) but tasm lacks the
  `CmpWordSiPtrImm8Sx`/`Imm16` variants. Add the `83 3C ii`
  and `81 3C lo hi` encodings to enable the peephole.

## Static char init, char as cond, typedef long alias

Fixtures `998` (`static char c = 'A'; return c;` — function-
local static char with non-zero init), `999` (`char c = 1;
if (c) return 7;` — char local as a boolean condition,
no explicit compare), `1000` (`typedef long Big; Big g =
100000L;` — typedef aliasing `long` and using the alias to
declare a long global with a wide initializer).

All three already work end-to-end:

- 998: function-local static char with init lands in `_DATA`
  (since the value is non-zero) as a `db 65` (`'A'`). Same
  shape as fixture 161/162 for int statics; the char
  variant uses the byte form. Codegen treats the static
  as a private global (DGROUP-relative addressing).
- 999: `if (c)` for a char local lowers as `cmp byte ptr
  [bp-1], 0`. The existing `emit_zero_test` local-Ident arm
  routes char-typed locals through the byte-form compare
  (fixture 536 covered the global flavor).
- 1000: `typedef long X;` registers `X` as an alias for
  `Type::Long`. At the global decl site `Big g = 100000L;`
  resolves `Big` via the typedef table and emits the long-
  init shape (`dw lo; dw hi` in `_DATA`, two FIXUPPs).
  Fixture 209 covered direct `long g = 100000L`; this
  confirms the typedef-routed form is byte-equivalent.

## Char shr const, char cmp int local, static-local init

Fixtures `995` (`char c = 16; return c >> 2;` — char right-
shift by a const, returned as value), `996` (`char c = 5;
int x = 5; if (c == x)` — char compared to int local, mixed
widths), `997` (`static int s = 42; return s;` — function-
local static with non-zero initializer).

All three already work end-to-end:

- 995: char unrolls into widen-then-shift — `mov al, [bp-1];
  cbw; sar ax, 1; sar ax, 1` (count=2). Promoted-to-int
  pattern matches fixture 121's `<<` sibling. The signed
  `sar` is chosen because char is signed by default in BCC.
- 996: char-vs-int compare widens the char operand to int
  first via cbw, then runs the standard `cmp ax, [bp-N]`.
  The char is the LHS — `emit_compare` doesn't see the
  char-vs-char fast-path (RHS is int), so the generic
  promote-and-compare path handles it. BCC emits `mov al,
  byte ptr [bp-1]; cbw; cmp ax, word ptr [bp-4]`.
- 997: static locals with non-zero init are emitted in
  `_DATA` rather than `_BSS` (since BSS only holds zero-
  initialized symbols). Same shape as fixture 161/162 —
  the static-local-with-init path was already covered;
  this confirms it for a non-zero value.

## Shift by 8, char struct field cmp, two-field struct add

Fixtures `992` (`int x = 1; return x << 8;` — shift by a
const that's > 3, forcing the CL load path), `993`
(`struct S { char c; } s; if (s.c == 'A')` — char struct
field compared to char-literal const), `994` (`struct S
{ int a; int b; } s; return s.a + s.b;` — local struct,
write both fields, read both and add).

All three already work end-to-end:

- 992: BCC unrolls shifts by 1, 2, or 3 into repeated `shl
  ax, 1`. For shift counts > 3 (or non-power-of-2 K), it
  emits `mov cl, K; shl ax, cl` — the CL load path. Our
  codegen already handled both shapes; this fixture pins
  the >3 path. Fixture 121 covered count=3.
- 993: char struct field cmp const lowers through the same
  byte-form memory compare as a char global: `cmp byte ptr
  DGROUP:_s+offset, K`. The chain-based compare peephole
  from batch 224 handles `s.c` for both byte-typed and
  word-typed leaves.
- 994: two struct field reads + add. BCC emits `mov ax,
  [bp+a]; add ax, [bp+b]` (or DGROUP-relative for globals).
  Our generic `Member` rvalue path supplies the operand
  source, and the generic binary-op emit handles the rest.

## Ptr local cmp zero, struct field var-RHS write, member cmp

Fixtures `989` (`int *p; p = &g; if (p == 0) return 1;` —
pointer local compared to zero in if), `990` (`s.x = v;`
with v a stack local — struct field assigned from non-
constant), `991` (`s.x = 5; if (s.x == 5)` — struct field
compared to constant in if).

989 already worked via the existing `if (var == 0)` zero-
test path — pointer locals route through the same
`cmp word ptr <var>, 0` shape as integer locals (the
`pointee.is_some()` branch in the local-Ident arm).

990 needed a small extension to `emit_member_assign`. The
existing path panicked on non-const RHS. Added an int-field
non-const arm: `emit_expr_to_ax(value); mov word ptr
<dest>, ax`. Same shape as BCC: `mov ax, [bp-N]; mov word
ptr DGROUP:_s, ax`. Restricted to non-char fields for now.

991 exposed a missing memory-direct compare peephole for
`<member-or-array> == const` against a global root. The
batch-220/221 peephole only covered stack-local roots.
Generalized that arm: when `try_lvalue_chain_addr` resolves
to a global root, emit `cmp word ptr DGROUP:_<name>+off, K`
(or byte form). Sibling of the local-root case, identical
mnemonic and immediate-handling. Now covers `s.x`, `s.b.x`,
`g.a[K]`, etc., on both globals and locals — every chain
that resolves to a constant memory address.

## Stack array elem `&=` const, elem-to-elem copy, var-RHS compound

Fixtures `986` (`int a[3]; ... a[1] &= 0x0F;` — stack int
array compound bitwise AND with const), `987` (`int a[3];
... a[2] = a[1];` — stack array elem copied from another
elem of the same array), `988` (`int a[3]; int x; ... a[1]
-= x;` — stack array compound sub with var RHS).

986 and 987 worked end-to-end:

- 986: the existing constant-RHS path in the array compound
  assign branch already covered the bitwise case — same
  `and word ptr [bp+(base+K*stride)], imm` shape as the
  add/sub arms, just with a different mnemonic.
- 987: the rvalue ArrayIndex path (batch 220) supplies the
  `[bp+(base+K*2)]` operand source for the RHS, and the
  assign-array-elem const-RHS-or-AX path (batch 222) stores
  AX into the LHS element. Two `[bp+N]` operands, one
  16-bit value moving through AX.

988 needed an extension to the array-compound-assign arm at
emit_array_compound_assign:~6670. The arm panicked on
non-const RHS for stack-local arrays. Added a non-const arm
mirroring the global-pointer-subscript compound path: load
RHS to AX, emit `<op> word ptr [bp+(base+K*stride)], ax`
where `<op>` is `add` / `sub` / `and` / `or` / `xor` based
on the operator. Same five-op family as the existing const-
RHS path; char-element non-const compound still panics
(no fixture yet). Mirrors BCC's actual shape: `mov ax,
[bp-8]; sub word ptr [bp-4], ax`.

## Stack array elem postinc, var-RHS write, mul-const

Fixtures `983` (`int a[3]; a[1]++;` — stack int array elem
postinc statement), `984` (`int a[3]; int x; a[0] = x;` —
stack array elem assigned from a stack local), `985`
(`return a[1] * 3;` — stack array elem times a small const).

983 and 985 worked end-to-end:

- 983: BCC emits `inc word ptr [bp+(base+K*stride)]` —
  memory-direct increment on the bp-relative element. Our
  existing array-postinc statement path already handles
  stack arrays (sibling of fixture 547's preinc form).
- 985: the rvalue path from batch 220 supplies the
  `[bp+(base+K*2)]` operand source; the generic `*=` arm
  unrolls `* 3` into `mov dx, 3; imul dx` after loading
  the array elem into AX. Same shape as a `local * 3`
  multiplication.

984 needed a small extension to the array-assign path. The
constant-indexed-array assign arm at `emit_array_assign:
~6046` already had a const-RHS store but panicked for
non-const RHS. Added a non-const arm for int/uint/pointer
leaf types: `emit_expr_to_ax(value); mov word ptr [bp+
elem_off], ax`. Same shape BCC emits for `a[0] = x` with x
a stack local — `mov ax, [bp-N]; mov [bp+elem_off], ax`.
Restricted to non-char leaves for now since a char-element
non-const store needs the AL detour (byte register +
narrow store); the panic message stays for that case.

## Stack array elem as bool, plus const, char return

Fixtures `980` (`if (a[1]) return 7;` — stack-array element
as a boolean test in if), `981` (`return a[2] + 100;` —
stack-array element added to a constant in return), `982`
(`char a[3]; ...; return a[2];` — char stack-array element
read and returned).

981 and 982 worked end-to-end via the batch-220 rvalue
ArrayIndex fallthrough — same `[bp+(base+K*stride)]`
operand source that 977 added, plus the existing
add-with-immediate and char-return paths.

980 hit `emit_zero_test`'s "non-ident boolean condition"
panic — the zero-test had arms for register-resident
deref, global-pointer subscript, and identifier targets,
but no arm for a stack-array element. Added one using the
same `try_lvalue_chain_addr` helper as the rvalue and
compare paths: when the cond is `ArrayIndex` whose root
is a stack-local array, emit `cmp <width> ptr [bp+
(base+K*stride)], 0` directly (byte for char arrays, word
for int). Two bytes vs the AX-detour, identical to BCC.

Three sites in codegen now share the chain-walk+local-
fold pattern: `resolve_operand_source` (batch 220),
`emit_compare` (batch 220), `emit_zero_test` (this batch).
A future refactor could factor the "local-stack-array
elem → bp-relative operand" computation into a single
helper, but each site needs slightly different output
(operand source vs cmp-vs-imm vs cmp-vs-0), so the dupe
is small and obvious.

## Stack array elem in rvalue + memory-direct compare

Fixtures `977` (`int a[3]; ...; return a[0] + a[1];` — two
stack-array element reads added together), `978` (`int
a[3]; ... if (a[1] == 10) return 1;` — stack-array element
compared to constant in an if-condition), `979` (char-array
sibling of 978).

977 needed an extension to the rvalue ArrayIndex arm in
`resolve_operand_source`. The existing arm at line ~10037
folds `g[K]` (global) through `try_lvalue_chain_addr` to a
`GlobalOffset`, but panicked for any non-global base.
Added a local-array fall-through: when the resolved root
is a stack-resident local, compute the bp-relative elem
offset (`base_off + total_off`) and return
`OperandSource::Local(elem_off)`. The downstream generic
`add ax, word ptr [bp+N]` shape already handles that
operand source.

978 / 979 exposed a missed compare peephole. BCC emits a
single memory-direct `cmp word ptr [bp+(base+K*stride)],
K` (3-byte form `83 7E dd ii`) where our codegen was
materializing the LHS into AX first (`mov ax, [bp-4]; cmp
ax, 10` — 6 bytes). Added a new arm in `emit_compare`
that, when LHS is an `ArrayIndex` whose root resolves to
a stack local, emits the byte- or word-form memory-direct
compare against the constant RHS. Same shape as the
existing int/char global memory-direct compare paths just
with `[bp+N]` instead of `DGROUP:_<name>`.

The leaf type from `try_lvalue_chain_addr` drives the
width: `is_char_like()` picks `cmp byte ptr ...,K`,
otherwise `cmp word ptr ...,K`. Saves 3 bytes per
compare on int arrays and 3 bytes on char arrays.

## `&&` of two compares, int double-init, array write/read

Fixtures `974` (`if (a > 0 && b > 0) return 1;` — `&&`
joining two comparisons), `975` (`b = a + a;` —
initializing an int from a binop), `976` (`a[0] = 5;
a[1] = 10; a[2] = 20; return a[1];` — write each element
of a stack-resident int array, then read one back).

All three worked end-to-end:

- 974: the `&&` condition lowers as two independent
  zero-tests with the false-target jump landing at the
  same skip label. BCC's pattern: each compare emits
  `cmp; jle <skip>` independently, the if-body runs only
  if both fall through. Our `emit_cond_branch` already
  threads the same false-slot through both subterms.
- 975: `b = a + a;` lowers to `mov ax, [bp-2]; add ax,
  [bp-2]; mov [bp-4], ax` — the same `add r16, r/m16`
  shape as `a + b` between two distinct locals, just
  with the same operand used twice. Sibling of fixture
  598 (`return x * x`).
- 976: writing to a constant-indexed int-array element
  lowers to `mov word ptr [bp+K*2], imm16` (or the imm8sx
  form for small constants). Three writes, then a read of
  one element via `mov ax, word ptr [bp+2]`. The bp-offset
  arithmetic is constant-folded by `try_const_array_offset`.

**Recorded findings (deferred):**

- **Struct field `++` as value** (`r = s.x++;`): parser
  panics "expected `;`, got `++`" at byte 79 — the postfix
  parser doesn't yet accept `++`/`--` after a `Member`
  expression. Needs an arm in the postfix loop to wrap a
  Member node in `Update { Post }`.
- **Char in for-loop bound** (`char c; for (c = 0; c < 5;
  c++)`): codegen compiles but produces a 6-byte-different
  OBJ. Two divergences from BCC: (1) BCC enregisters the
  char in BL, we use DL — register-allocation policy
  difference; (2) BCC's `inc bl` is one byte, ours goes
  through the AL detour (`mov al, bl; inc al; mov bl, al`)
  which is 4 bytes. (2) is a peephole we could add: when
  `++c` targets a byte-register-resident char and the
  result isn't observed (or the use can read the register
  directly), emit `inc <reg8>` directly. Needs the
  expression-context update path to detect "side-effect-
  only" use, since the AL detour is correct for `r = c++`.
- **Char self-binop assign** (`c = c + 1;` with char c):
  codegen panics "non-constant char init/assign not yet
  supported". BCC special-cases this as the AL-detour
  shape (same as `c += 1`). Needs the char-assign path to
  recognize `c = c <op> K` and route through the compound
  path.

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
