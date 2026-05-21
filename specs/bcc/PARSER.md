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

## Int shift threshold pinned: N=1,2,3 unrolled `d1 e0`; N≥4 cl-form `b1 N / d3 e0`; var shift = `8a /4 / d3 e0`

Fixtures `2273` (N=2), `2274` (N=3), `2275` (var
shift) pin down BCC's int shift threshold.

- `2273` (**int shift by 2**): 2 unrolled `d1 e0`
  (4 bytes total).
- `2274` (**int shift by 3**): 3 unrolled `d1 e0`
  (6 bytes total).
- `2275` (**int shift by var**): cl-form with
  byte load of the shift count:
  ```
  mov ax, [x]
  8a 4e fc                  ; mov cl, [n] (low byte of int)
  d3 e0                     ; shl ax, cl
  ```
  Uses `8a /4 [m]` = mov r8, r/m8 to load only the
  low byte. On 8086, `shl reg, cl` doesn't mask
  CL (unlike 286+), so shifts > 15 zero the int.

**Refined int shift threshold (FINAL)**:
| N | Form | Bytes |
|---|------|-------|
| N=1 | `d1 /4 reg` | 2 |
| N=2 | 2 × `d1 /4 reg` (unrolled) | 4 |
| N=3 | 3 × `d1 /4 reg` (unrolled) | 6 |
| N=4+ | `b1 N / d3 /4 reg` (cl-form) | 4 |
| Var | `8a /4 [n] / d3 /4 reg` | 5+ |

So N=4 is the breakeven: 4 unrolled = 8 bytes vs
cl-form = 4 bytes. BCC chooses the shorter option.

**Right shift signed (SAR) and unsigned (SHR)**
likely use the same threshold rule, just with `/7`
(SAR) or `/5` (SHR) ModR/M instead of `/4` (SHL).

**Variable shift quirk** (8086):
- 8086's `shl reg, cl` uses CL as-is without
  masking (in contrast to 286+ which masks to mod
  32)
- For int (16-bit) shifts where CL > 15, result
  is 0 (all bits shifted out)
- BCC loads only the low byte of the count
  variable via `mov cl, [m]` (8-bit move)
- Even if count > 255 stored in an int, only the
  low 8 bits reach CL

For the Rust reimplementation:
- For shift by const N: emit N unrolled `d1 /4`
  if N ≤ 3, else `b1 N / d3 /4` cl-form.
- For shift by var: load low byte via `8a /4`,
  then `d3 /4` cl-form.

## No rotate-pattern recognition (emits 2 shifts + or); int shift by 4+ uses `b1 N / d3 e0` cl-form; chained ternary nests

Fixtures `2270` (rotate emulation), `2271`
(popcount), `2272` (chained ternary) probe
optimization opportunities BCC does/doesn't take.

- `2270` (**rotate via shifts**): `(x << 4) | (x
  >> 12)` is NOT recognized as a rotate-left-by-
  4. BCC emits two separate shifts + an OR:
  ```
  mov ax, si              ; copy of x
  mov cl, 4
  shl ax, cl              ; x << 4
  mov dx, si              ; second copy
  mov cl, 12
  shr dx, cl              ; x >> 12 (logical, unsigned)
  or ax, dx               ; combine
  ```
  Uses `b1 N / d3 e0` (cl-form) instead of N
  unrolled `d1 e0` because **N ≥ 4 for ints**:
  - N=1: `d1 /4 reg` (2B)
  - N=2,3: probably 2-3 unrolled (4-6B; needs
    verifying)
  - N ≥ 4: `b1 N / d3 /4 reg` (4B, cl-form wins)
- `2271` (**popcount loop**): straightforward
  per-bit loop. No special instruction or pattern
  recognition. Same codegen any compiler without
  intrinsics would emit:
  ```
  while_top:
    or si, si          ; test x != 0
    je end
    mov ax, si
    and ax, 1          ; bit 0
    add di, ax         ; n += bit
    shr si, 1          ; x >>= 1
    jmp while_top
  end:
  ```
- `2272` (**chained ternary**): `(c1) ? a : (c2)
  ? b : c` lowers to right-associative nested
  if-else with cmp+jcc+value+jmp per level:
  ```
  ; (x > 0) ? 1 : (x < 0) ? -1 : 0
  test x
  jle outer_false
  mov ax, 1 / jmp end
  outer_false:
    test x (again)
    jge zero
    mov ax, -1 / jmp end
  zero:
    xor ax, ax
  end:
  ```

**Pattern recognition NOT performed in BCC**:
- ROL/ROR via shift+or
- popcount (single-instruction on 286+ via lookup
  table or via BSF/BSR)
- min/max via cmp + cmov (cmov is 386+ anyway)
- bit-test idioms like `(x & (1 << n)) != 0`
- swap via XOR trick
- abs via cmp + neg

**Int shift threshold** (vs long):
| Shift type | N | Form |
|------------|---|------|
| Int shift | 1 | `d1 /4 reg` (2B) |
| Int shift | 2-3 | unrolled or cl-form |
| Int shift | 4+ | `b1 N / d3 /4 reg` (cl-form, 4B) |
| Int shift | var | `mov cl, [src] / d3 /4 reg` |
| Long shift | 1 | inline `shl dx, 1 / rcl ax, 1` (4B) |
| Long shift | 2+ | N_LXLSH@ helper |

For the Rust reimplementation:
- Don't pattern-match rotate or popcount; emit
  the equivalent series of primitives.
- For int shifts: emit N unrolled `d1 e0` for N <
  4, else cl-form.
- For chained ternary: emit nested if/else
  pattern per ternary level.

## Large frames use `81 ec NN NN` + bp+disp16 (`8d 86 disp16`); ptr+N scales at compile time; 8 args = `add sp, 16` cleanup

Fixtures `2267` (large local frame), `2268` (8
args fn), `2269` (ptr arithmetic) cover scaling
mechanics for large offsets and pointer math.

- `2267` (**large frame, disp16 form**): for
  `int a[100]` (200 bytes), prologue uses imm16
  sub form and ModR/M uses bp+disp16:
  ```
  ; Prologue:
  push bp / mov bp, sp
  81 ec c8 00              ; sub sp, 200 (imm16 form, 4B)
  push si
  
  ; Access a[i]:
  mov bx, si / shl bx, 1
  8d 86 38 ff              ; lea ax, [bp + 0xFF38] (= bp - 200)
                            ; ModR/M /86 disp16 = bp+disp16 form (4B vs 3B)
  add bx, ax               ; &a[i]
  mov [bx], si             ; a[i] = i
  ```
- `2268` (**8 args function**): arg offsets fit
  in disp8 (max +18 for 8th arg):
  ```
  ; In sum8(a,b,c,d,e,f,g,h):
  mov ax, [bp+4]            ; a
  add ax, [bp+6]            ; + b
  ...
  add ax, [bp+18]           ; + h (max disp8 for typical fn)
  
  ; Caller after call:
  add sp, 16                 ; cleanup 8 args × 2 bytes
  ```
  For a fn with > 60 args (~127B offsets), the
  callee would start using disp16 form for the
  later args.
- `2269` (**ptr arithmetic `p + 2` / `p - 2`**):
  scaled by sizeof at compile time:
  ```
  mov si, &a[5]
  
  ; q = p + 2 (= +4 bytes for int*):
  mov ax, si / add ax, 4
  
  ; r = p - 2 (= -4 bytes):
  mov ax, si / add ax, 0xFFFC   (= -4 signed)
  
  ; q - r (element diff):
  sub ax, [r]
  cwd / mov bx, 2 / idiv bx     ; / sizeof
  ```

**ModR/M displacement forms** (8086):
| Form | Bytes | Range | Use |
|------|-------|-------|-----|
| `/06 disp16` | 3 | absolute | direct addressing |
| `/06 disp8` | (N/A; no disp8 for direct) | - | - |
| `/46 disp8` | 2 | -128 to +127 | small bp/bx offsets |
| `/86 disp16` | 3 | full 16-bit | large bp/bx offsets |

For BCC, threshold for switching disp8 → disp16
is when the offset cannot fit in signed 8 bits.
ARRAY bases inside large fns commonly trigger
this (e.g., `bp + 0xFF38` for a frame > 128 bytes).

**Pointer arithmetic encoding**:
- `p + N` (N const): `add ax, N*sizeof` (one inst)
- `p + var` (var dynamic): compute `var*sizeof`
  via shifts, then `add`
- `p - N` (N const): `add ax, -(N*sizeof)` (the
  -N is sign-extended imm16)
- `p++` / `++p`: `add ax, sizeof` (or `inc` × N
  if size ≤ 2)
- `p - q` (both ptr): `sub` byte diff, then
  `cwd / idiv sizeof`

For the Rust reimplementation:
- Track frame size at fn entry; emit `81 ec`
  imm16 form if > 127.
- Use `bp+disp16` ModR/M when offset > 127.
- Scale ptr arith by sizeof at compile time.

## Per-fn static vars get distinct `_DATA` slots; `static` fn = no PUBDEF; `int a[]` ≡ `int *a` byte-identical

Fixtures `2264` (per-fn statics), `2265` (static
fn internal linkage), `2266` (array vs ptr arg)
cover function-scope storage and parameter
equivalence.

- `2264` (**per-fn static vars**): each fn's
  `static int counter` gets its own slot in
  `_DATA`. No mangling needed since they're not
  exported:
  ```
  _DATA layout:
    [00 00]    ; counter for next_a (init 0)
    [64 00]    ; counter for next_b (init 100)
  
  next_a body: inc word [0] / mov ax, [0]
  next_b body: inc word [2] / mov ax, [2]
  ```
  Statics behave like global ints in `_DATA`,
  just not exported (no PUBDEF).
- `2265` (**`static` function**): no PUBDEF
  entry — symbol not exported across TUs. Body
  emitted normally in `_TEXT`. Intra-TU callers
  use `e8 [rel]` since target's offset is known
  at compile time.
- `2266` (**`int a[]` ≡ `int *a` as parameter**):
  BYTE-IDENTICAL function bodies for sum_arr and
  sum_ptr. Per C standard, array parameters
  decay to pointers — no distinction in codegen.

**Storage class & linkage summary**:
| Storage | Linkage | PUBDEF | EXTDEF | `_DATA` slot |
|---------|---------|--------|--------|--------------|
| `int x = 5;` (global init) | external | ✓ | ✗ | ✓ initialized |
| `int x;` (global uninit) | external | ✓ | ✗ | ✓ in `_BSS` |
| `static int x = 5;` (file scope) | internal | ✗ | ✗ | ✓ initialized |
| `static int x;` (file scope) | internal | ✗ | ✗ | ✓ in `_BSS` |
| `static int x = 5;` (block scope) | none | ✗ | ✗ | ✓ initialized |
| `static int x;` (block scope) | none | ✗ | ✗ | ✓ in `_BSS` |
| `int x;` (local) | none | ✗ | ✗ | ✗ stack |
| `extern int x;` | external | ✗ | ✓ | ✗ (defined elsewhere) |
| `int f(...)` (global) | external | ✓ | ✗ | n/a (text) |
| `static int f(...)` | internal | ✗ | ✗ | n/a (text) |
| `extern int f(...);` | external | ✗ | ✓ if used | n/a |

**Parameter type equivalences** (C standard):
| Declared form | Actual passed | Same as |
|---------------|---------------|---------|
| `int a[]` | int * (pointer) | `int *a` |
| `int a[10]` | int * (pointer; 10 ignored) | `int *a` |
| `int a[][10]` | pointer to int[10] | `int (*a)[10]` |
| `int f(int)` | function ptr to f | `int (*f)(int)` |

For the Rust reimplementation:
- Per-fn statics: emit unique `_DATA` slots; track
  scope via name mangling internally (no PUBDEF).
- `static` fn: emit body, skip PUBDEF.
- Parameter decay: treat `int a[]` and `int *a`
  identically in codegen.

## `-N` = per-fn `cmp [___brklvl], sp` + N_OVERFLOW@ helper; `-A` codegen-identical; `-r-` disables enregistration

Fixtures `2261` (-N), `2262` (-A), `2263` (-r-)
cover three CLI flags affecting codegen.

- `2261` (**`-N` stack overflow check**): each
  fn's prologue gets a check:
  ```
  push bp / mov bp, sp / sub sp, N
  ; -N check:
  cmp [___brklvl], sp    ; 39 26 00 00
  jb +3                   ; 72 03 — branch past helper if OK
  call N_OVERFLOW@        ; e8 00 00 — overflow → invoke handler
  ; ... function body ...
  ```
  Adds EXTDEFs for `N_OVERFLOW@` and `___brklvl`.
  Overhead: ~9 bytes per fn.
- `2262` (**`-A` ANSI mode**): codegen identical
  to default. The flag enforces strict ANSI
  conformance during parsing (rejects K&R-style
  syntax), but valid C compiles to the same
  bytes.
- `2263` (**`-r-` disable register vars**):
  forces all variables to memory:
  ```
  ; Without -r-: i in SI, sum in DI (no stack use)
  ; With -r-:
  mov word [bp-4], 0       ; sum = 0 in memory
  mov word [bp-2], 1       ; i = 1 in memory
  jmp test
  body:
    mov ax, [i]
    add [sum], ax
    inc word [i]
  test:
    cmp word [i], 10
    jle body
  ```

**Stack overflow check details** (`-N` flag):
- `___brklvl` is a global linker symbol (typically
  set by the startup code to point at the bottom
  of the stack-safe region, just above the heap)
- Check happens AFTER locals are allocated (so SP
  reflects the new frame size)
- `cmp [___brklvl], sp` followed by `jb` — note
  the operand order: `cmp A, B` computes A - B
  and sets flags. JB taken if A < B (unsigned)
- If SP went BELOW brklvl, the stack has grown
  into the heap → call `N_OVERFLOW@` which
  typically aborts the program
- Adds ~9 bytes per function

**ANSI mode `-A` impact**:
- Disables K&R syntax acceptance (no implicit
  `int` returns, no untyped fn args, etc.)
- Disables Borland extensions (interrupt, near/
  far if not preceded by `_`)
- Codegen byte-identical for ANSI-conforming source

**`-r-` impact**:
- Suppresses all enregistration including loop
  counters and accumulators
- Useful for debug builds (stable memory addresses)
- Larger code, slower execution

**BCC CLI flag catalogue** (codegen-affecting):
| Flag | Effect |
|------|--------|
| `-c` | Compile only, no link |
| `-ms`/`-mc`/`-mm`/`-ml`/`-mh` | Memory model |
| `-O` | Strip eb 00 no-ops |
| `-d` | Merge duplicate string literals |
| `-1` | 80186 instructions (ENTER/LEAVE/shl-imm) |
| `-2` | 80286 instructions |
| `-K` | char defaults to unsigned |
| `-N` | Stack overflow check |
| `-A` | Strict ANSI mode (no Borland exts) |
| `-r-` | Disable register variables |
| `-f-` | No floating point linkage |
| `-G` | Optimize for speed (vs size) |
| `-D NAME=val` | Define preprocessor macro |
| `-I path` | Add include path |

For the Rust reimplementation:
- `-N`: emit per-fn brklvl check + N_OVERFLOW@.
- `-A`: parser strictness only (no codegen
  difference for valid ANSI input).
- `-r-`: skip enregistration; emit all vars to
  memory.

## Nested-block scope = separate slots (shadowing); large arrays = `sub sp, N`; enregistration is conservative

Fixtures `2258` (nested-block shadowing), `2259`
(large local arr), `2260` (10 locals) cover
stack-frame and enregistration mechanics.

- `2258` (**nested block scoping**): each `{`
  ... `}` introduces a new scope. Shadowing
  variables get distinct stack slots. The
  innermost was enregistered (SI):
  ```
  ; int x = 1 (outer)
  mov word [bp-2], 1
  
  ; { int x = 2; (middle)
  mov word [bp-4], 2
  
  ; { int x = 3; (innermost — REGISTER)
  mov si, 3
  
  ; innermost x = x + 10
  mov ax, si / add ax, 10 / mov si, ax
  
  ; }} (blocks close — no cleanup needed)
  
  ; return outer x
  mov ax, [bp-2]
  ```
  Outer x is at [bp-2], middle at [bp-4], inner
  in SI. After inner blocks close, control just
  returns to outer scope — no stack cleanup
  needed since BCC always allocates max frame
  at fn entry.
- `2259` (**large local array**): `int a[50]`
  allocates 100 bytes via:
  ```
  add sp, -100         ; 83 ec 64 (= sub sp, 100)
  ```
  Same prologue pattern, just larger immediate.
  8086's 16-bit SP supports frames up to 64KB.
- `2260` (**10 locals all enregistration-eligible
  but in memory**):
  ```
  ; 10 ints, allocated 20 bytes:
  add sp, -20
  
  ; All 10 init stores to memory:
  mov word [bp-2], 1
  mov word [bp-4], 2
  ...
  mov word [bp-20], 10
  
  ; All 10 loads + adds:
  mov ax, [bp-2]
  add ax, [bp-4]
  ...
  add ax, [bp-20]
  ```
  NO enregistration despite many candidates.

**BCC enregistration heuristic** (refined):
- Variables marked `register`: enregistered if free
  reg available
- Loop counters / iterators: enregistered (e.g.
  for (i=...; ...; i++))
- Innermost shadowing variable: enregistered when
  outer scope has unused regs
- Plain mostly-unused locals: kept in memory
- Variables whose address is taken (`&x`): always
  memory
- Variables used > 2 times: usually enregistered
- "Hot" path variables: prioritised for SI/DI

Heuristic appears to be **conservative** — prefer
memory over registers unless there's a clear
benefit. This makes codegen predictable but
sometimes leaves performance on the table.

**Stack frame allocation summary**:
| Local count | Stack alloc |
|-------------|-------------|
| 0 locals | No `sub sp` (or `sub sp, 0`) |
| 1-2 ints | `sub sp, 4` (single 8-bit imm via `83`) |
| 3-63 ints | `sub sp, N` (`83 ec N` 3B) |
| 64+ bytes | `add sp, -N` 16-bit imm form (`81 c4 NN NN`) |

For the Rust reimplementation:
- Track scope nesting; shadowing → new slots.
- Aggregate locals at fn entry; emit `sub sp, N`
  prologue.
- Enregistration: register-qualified + loop
  counters as priorities; rest in memory.

## Recursion = regular call (no special handling); mutual recursion via fwd-decl; NO tail-call elimination

Fixtures `2255` (factorial), `2256` (mutual
recursion via fwd decl), `2257` (tail-call check
— BCC doesn't TCE) cover function-call recursion
patterns.

- `2255` (**recursive factorial**): just a normal
  `call near` to self. Each invocation gets a
  fresh BP frame via the standard prologue:
  ```
  ; In fact(int n):
  cmp si, 1                  ; n <= 1?
  jg L_recurse
  mov ax, 1                  ; base case
  jmp end
  L_recurse:
    mov ax, si
    dec ax                   ; n - 1
    push ax
    e8 [rel]                  ; call _fact (intra-TU)
    pop cx
    imul si                  ; ax *= n
  end:
  ```
  Recursion "just works" via the call/ret/BP
  discipline.
- `2256` (**mutual recursion via fwd decl**): the
  forward declaration `int is_odd(int n);` lets
  BCC's parser know is_odd exists when compiling
  is_even. Both fns end up in the same `_TEXT`
  segment with intra-TU `e8 [rel]` calls (filled
  in at compile-time once all symbols seen).
  No EXTDEF needed for forward intra-TU refs.
- `2257` (**no tail-call elimination**): `return
  helper(x)` lowers to full call + epilogue:
  ```
  ; In wrapper(int x): return helper(x);
  push word [bp+4]            ; arg
  e8 [rel]                    ; call _helper
  pop cx                       ; cleanup
  ; (no special handling — standard epilogue)
  mov sp, bp
  pop bp
  ret
  ```
  BCC does NOT collapse this into `jmp _helper`.
  Consistent with simple non-optimizing compiler.

**Recursion / call optimizations in BCC**:
| Optimization | BCC behavior |
|--------------|--------------|
| Tail-call elimination | Not performed |
| Tail-recursion → loop | Not performed |
| Inlining | Not performed |
| Common-subexpression elimination | Not performed |
| Dead-code elimination | Not performed |
| Constant propagation across blocks | Not performed |
| Loop unrolling | Not performed (except const-shift unroll for `<< 1` etc.) |

So calls have no special collapsing — every C
function call results in a real machine call,
prologue, epilogue, ret. Recursion goes through
the same call mechanism. Stack depth = recursion
depth × (BP saved + locals + ret addr).

For the Rust reimplementation:
- Recursive calls: emit standard call
  instruction; no special handling.
- Tail calls: do NOT collapse to jmp.
- Mutual recursion: track forward references in
  the symbol table; backpatch rel16 at EOF.

## `typedef fn ptr` is parse-time alias; multi-fn TU = one _TEXT seg + per-fn PUBDEF; extern fn = EXTDEF + FIXUPP'd call

Fixtures `2252` (typedef fn ptr), `2253` (5 fns
in one TU), `2254` (extern decl no body) cover
function-level translation-unit organization.

- `2252` (**`typedef int (*BinOp)(int,int)`**):
  pure parse-time alias. Codegen identical to
  using the raw type:
  ```
  ; In apply(BinOp f, int a, int b):
  push word [bp+8]                ; b
  push word [bp+6]                ; a
  ff 56 04                        ; call near [bp+4] — through f
  ```
- `2253` (**multi-fn same TU**): all fns share
  one `_TEXT` segment in the small model. Each
  fn gets its own PUBDEF entry. Bodies emitted
  in declaration order; PUBDEF emission order
  appears to be based on internal symbol table
  layout (not strict declaration order). The
  caller's relative calls (`e8 [rel]`) are filled
  in directly at compile time since all targets
  are intra-segment.
- `2254` (**extern fn decl no body**): only an
  EXTDEF entry; no PUBDEF, no body. Call sites
  use FIXUPP'd `e8 00 00`:
  ```
  e8 00 00                ; call near (rel16)
                          ; FIXUPP relocates the rel16 at link time
  ```
  Linker assumes the extern target ends up in
  the same code segment as the caller (small/
  compact model). For medium/large/huge with
  extern, would use `9a [off][seg]` full far
  call instead.

**Translation-unit symbol summary**:
| Declaration | PUBDEF | EXTDEF | Body | Notes |
|-------------|--------|--------|------|-------|
| Defined globally | ✓ | ✗ | ✓ | Exported |
| Defined `static` | ✗ | ✗ | ✓ | Local-only |
| `extern` declared, used | ✗ | ✓ | ✗ | Linker resolves |
| `extern` declared, unused | ✗ | ✗ | ✗ | Elided |
| `typedef` | ✗ | ✗ | ✗ | Parse-time only |

**Multi-fn TU emission order**:
- Bodies in `_TEXT`: declaration order
- PUBDEFs: appears to be hash-table iteration
  order (not strict declaration order)
- EXTDEFs: appears in use-order

For the Rust reimplementation:
- Maintain a per-TU symbol table.
- Emit one `_TEXT` segment per TU containing all
  global fn bodies in declaration order.
- Emit PUBDEFs for non-static globals.
- Emit EXTDEFs for referenced undefined symbols.
- Treat typedef as a parse-time alias only.

## In small model: `near` no-op; `far` data ptr = 4B + LES + ES override; `far` fn = `push cs / call near` + `cb`

Fixtures `2249` (near in small), `2250` (far data
ptr in small), `2251` (far fn in small) probe
explicit pointer-size keywords.

- `2249` (**`near` in small**): byte-identical to
  default. Pointer is 2 bytes, no segment
  involvement. `near` qualifier is a no-op.
- `2250` (**`far` data ptr in small**): brings
  huge-style 4-byte pointer access into small
  model:
  ```
  ; Construct p (far ptr = offset + segment):
  lea ax, [x]
  mov [p.seg], ss            ; 8c 56 fc — store SS as segment
  mov [p.off], ax
  
  ; Dereference *p:
  les bx, [p]                ; c4 5e fa — load offset+segment
  mov ax, es:[bx]            ; 26 8b 07 — with ES override (3B)
  ```
  Cost: 6 bytes for ptr (vs 2), 3-5 bytes per
  deref (vs 2-3).
- `2251` (**`far` fn in small, same TU**): the
  far fn has `cb` (retf) instead of `c3`. The
  caller within the same translation unit uses
  the **`push cs / call near`** intra-CS
  optimization (4 bytes), same as medium model's
  default:
  ```
  ; In _helper (far fn):
  ...
  5d cb              ; pop bp / retf
  
  ; Caller (same TU):
  0e                ; push cs
  e8 NN NN          ; call near (rel16)
  ```

**Pointer-size qualifier summary** (in small model):
| Qualifier | Pointer | Deref | Notes |
|-----------|---------|-------|-------|
| (default) | 2B near | `[bx]` or `[bp+d]` 2-3B | DS implicit |
| `near` | 2B near | Same as default | No-op |
| `far` | 4B (off+seg) | `les / mov es:[bx]` 5B | Explicit segment |
| `huge` | 4B + normalised | (varies) | Normalised after arith |

**Far fn call form depends on whether target is
intra-segment**:
- Same TU + intra-CS: `push cs / call near` (4B)
- Different TU (extern): full `9a` (5B with seg
  FIXUPP)

**8086 segment override prefixes**:
| Prefix | Override | Use case |
|--------|----------|----------|
| 0x26 | ES | far pointer access |
| 0x2E | CS | code reads (rare) |
| 0x36 | SS | (rare; default for [bp]) |
| 0x3E | DS | (default; usually elided) |
| 0x64 | FS | (286+ only) |
| 0x65 | GS | (286+ only) |

For BCC 2.0 8086 target, only `26` (ES) is
emitted, for far data access.

For the Rust reimplementation:
- Track per-pointer model (near/far/huge).
- Far ptr load: emit `c4 [m]` (LES).
- Far ptr deref: emit segment-override prefix
  `26` before the access.
- Far fn intra-TU: emit `push cs / call near`.

## `pascal` = UPPERCASE name + L-to-R push + `ret N` callee cleanup; `interrupt` = save all + IRET; `cdecl` = default

Fixtures `2246` (pascal), `2247` (interrupt),
`2248` (explicit cdecl) pin the calling
conventions.

- `2246` (**pascal**):
  - Symbol: **`PSUM`** (UPPERCASE, NO underscore
    prefix) — vs cdecl `_psum`
  - Args pushed **LEFT-TO-RIGHT** at call site
    (vs cdecl R-to-L)
  - **Callee cleans up** via `ret N`:
    ```
    c2 04 00         ; ret 4 (callee pops 4 bytes)
    ```
  - Caller emits NO cleanup after call
- `2247` (**interrupt**): completely different
  function structure:
  ```
  ; Prologue (9 pushes):
  push ax / push bx / push cx / push dx
  push es / push ds
  push si / push di / push bp
  
  ; Fix up DS to point at this module's data:
  mov bp, DGROUP
  mov ds, bp
  
  ; Standard frame setup:
  mov bp, sp
  
  ; ... body ...
  
  ; Epilogue (9 pops in reverse):
  pop bp / pop di / pop si
  pop ds / pop es
  pop dx / pop cx / pop bx / pop ax
  
  ; IRET (not ret):
  cf
  ```
- `2248` (**explicit cdecl**): byte-identical to
  default — `_csum`, R-to-L push, caller cleanup.
  The keyword is a no-op confirming default.

**Calling convention summary** (final):
| Convention | Name | Push order | Cleanup | Return |
|------------|------|------------|---------|--------|
| `cdecl` (default) | `_funcname` | R-to-L | Caller (`add sp` / `pop cx`) | `c3` (ret near) or `cb` (ret far) |
| `pascal` | `FUNCNAME` (upper, no `_`) | L-to-R | Callee (`c2 NN 00` ret N) | `c2 NN 00` |
| `interrupt` | `_funcname` | (no args usual) | (full reg save/restore) | `cf` (IRET) |

**Pascal symbol naming**: the symbol table entry
for `psum` declared as `pascal` shows `PSUM`
(uppercase). Linker likely matches case-
sensitively, so callers must agree (typically
both sides have the same `pascal` declaration).

**Interrupt fn details**:
- Saves AX, BX, CX, DX (data regs)
- Saves ES, DS (segment regs)
- Saves SI, DI (index regs)
- Saves BP (frame ptr)
- Restores DS to module's DGROUP (since interrupts
  fire with caller's DS)
- Returns with IRET (pops IP, CS, flags)

For the Rust reimplementation:
- Track calling convention attribute per fn.
- Pascal: emit UPPERCASE PUBDEF symbol + `c2 NN
  00` ret + L-to-R caller pushes.
- Interrupt: emit save-all prologue + IRET +
  DGROUP DS fixup.
- cdecl: default; no special handling.

## `volatile`/`const` accepted but no-ops at codegen; `register` is effective (enregisters)

Fixtures `2243` (volatile), `2244` (const), `2245`
(register) test C qualifier handling.

- `2243` (**volatile**): emits **two separate
  loads** of x for `a = x; b = x;`:
  ```
  mov ax, [x] / mov [a], ax
  mov ax, [x] / mov [b], ax    ; reloaded (not cached)
  ```
  But BCC doesn't do CSE/load-caching anyway, so
  this is what would happen WITHOUT volatile too.
  Effectively a no-op at codegen.
- `2244` (**const local int**): N is given a
  normal stack slot, loaded from memory for
  comparison:
  ```
  mov word [N], 10
  ; ... later:
  cmp si, [N]                  ; not "cmp si, 10"
  ```
  BCC does NOT fold const-qualified variables to
  compile-time literals. Const is purely a type-
  system marker (for diagnostic warnings).
- `2245` (**register int**): variable goes into
  SI (or DI) — effective:
  ```
  mov si, 1                    ; i = 1 (in SI register)
  ; loop body uses SI directly
  ```
  Confirms that `register` is the ONE qualifier
  that actively affects codegen.

**C qualifier handling summary**:
| Qualifier | Codegen impact |
|-----------|----------------|
| `volatile` | None (BCC doesn't cache anyway) |
| `const` | None (BCC doesn't fold) |
| `register` | Hints enregistration (SI/DI/CX/...) |
| `static` | Changes symbol export (no PUBDEF) |
| `extern` | Changes symbol export (EXTDEF, no slot) |
| `auto` | Default for locals (no effect) |
| `near` | Forces near ptr (2B) in non-small models |
| `far` | Forces far ptr (4B) in non-huge models |
| `cdecl` | Default calling convention |
| `pascal` | Reverses arg order; callee cleanup |
| `interrupt` | Saves/restores all regs; iret |

So most modifiers don't change codegen at all —
they affect typechecking or symbol-table state.
Only `register`, `near`/`far`, and the calling
conventions actually shape code emission.

**Why volatile is a no-op in BCC**:
BCC is a simple compiler that performs:
- Parse-time constant folding
- Parse-time identity folding (x+0, x*1)
- Parse-time pow-2 strength reduction
- Per-statement register allocation

It does NOT perform CSE, DCE, loop hoisting, or
load forwarding. So `volatile` has nothing to
suppress.

For the Rust reimplementation:
- Track qualifiers in the type system.
- Honor `register` for enregistration hint.
- `volatile`/`const` codegen = same as without.

## No overflow check (silent wrap); `ptr - ptr` = byte-diff / sizeof via idiv; missing return = AX undefined

Fixtures `2240` (int overflow), `2241` (ptr - ptr
difference), `2242` (function without return)
cover three undefined-behavior edge cases.

- `2240` (**int overflow silent wrap**): standard
  `add ax, [y]` — no overflow check, no special
  jcc. Pure 8086 modular ALU semantics. For
  32000 + 1000 = 33000, result wraps to -32536
  (signed) or 33000 (unsigned interpretation).
- `2241` (**`ptr - ptr` = element count**): emits
  byte-diff then divide by sizeof:
  ```
  mov ax, [a]
  sub ax, [b]              ; ax = byte difference
  mov bx, 2                 ; sizeof(int)
  cwd                       ; sign-extend for idiv
  idiv bx                   ; ax = byte_diff / sizeof
  ```
  Uses SIGNED division because the difference
  can be negative (if a < b). Result is the
  number of ELEMENTS between the pointers.
- `2242` (**function missing return**): callee
  just falls through to epilogue without setting
  AX. **Whatever was in AX at fall-through point
  becomes the "return value"**. No warning, no
  zero-init, no nop. For `noret(5)` with `y = x*2`,
  AX happens to be 10 after `shl ax, 1`, so the
  caller sees 10.

**Undefined-behaviour summary** (BCC tactics):
| UB scenario | BCC behavior |
|-------------|--------------|
| Signed int overflow | Silent wrap (raw `add`) |
| Unsigned int overflow | Silent wrap (same instruction) |
| Long overflow | Silent wrap (`add/adc`) |
| Float overflow | FPU NaN/inf (FPU handles) |
| Missing return (non-void fn) | AX = whatever was last there |
| `ptr - ptr` (different arrays) | (UB) — same idiv mechanism, garbage |
| Null pointer deref | (UB) — no check; reads/writes addr 0 |
| Division by zero | INT 0 (8086 trap) |
| Stack overflow | `N_OVERFLOW@` if `-N`, else silent corruption |

So **BCC is a "trust the programmer" compiler** —
no UB checks, no defensive code, no warnings for
fall-through. The only runtime check is the
optional `-N` stack overflow guard.

For the Rust reimplementation:
- Emit raw `add/sub/mul` for int arithmetic.
- `ptr - ptr`: emit byte-diff then `cwd / idiv
  sizeof`.
- Function without return: emit epilogue
  unchanged. Do NOT zero AX or warn.

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

## Comma op = sequential statements; `op= imm16` = `81 /N imm16`; 2D array arg decays to ptr (stride at compile time)

Fixtures `2234` (comma operator), `2235` (bitwise
compound assignment), `2236` (2D arr as fn arg)
cover three orthogonal mechanisms.

- `2234` (**comma operator**): each subexpression
  evaluated left-to-right for side effects; only
  the LAST expression's value is the comma
  expression's value:
  ```
  ; (a = 5, b = 10, a + b)
  mov [a], 5            ; side effect 1
  mov [b], 10           ; side effect 2
  mov ax, [a]
  add ax, [b]           ; value of the whole expression
  ```
- `2235` (**bitwise compound assignment to reg**):
  uses `81 /N imm16` form (4 bytes):
  ```
  81 ce 0f 00            ; or si, 0x000F (/1 = OR)
  81 e6 f0 ff            ; and si, 0xFFF0 (/4 = AND)
  81 f6 aa aa            ; xor si, 0xAAAA (/6 = XOR)
  ```
  ModR/M `/N` selects the operation; reg field
  selects the register (ce=SI for /1, e6=SI for
  /4, f6=SI for /6).
- `2236` (**2D array as fn arg**): **decays to
  near pointer**! Only 2 bytes pushed (offset).
  Inside callee, compile-time stride still
  computes correct offsets:
  ```
  ; In main:
  push offset(g)              ; just the offset
  call _sum
  
  ; In sum(int m[3][3]):
  mov si, [bp+4]              ; the near ptr
  mov ax, [si]                ; m[0][0] (offset 0)
  add ax, [si+8]              ; m[1][1] (i=1*3*2+1*2 = 8)
  add ax, [si+16]             ; m[2][2] (offset 16)
  ```

**Array-arg decay rules** (refined):
| C type | Passed as | Bytes |
|--------|-----------|-------|
| `int a[]` (1D) | near `int *` | 2 |
| `int a[3][3]` | near pointer | 2 |
| `int (*a)[3]` (ptr-to-row) | near pointer | 2 |
| `int **a` (ptr-to-ptr) | near pointer | 2 |
| `char *a[]` | near pointer (to array of ptrs) | 2 |
| Struct (>4B) | full struct via N_SPUSH@ | sizeof |
| Struct (≤4B) | per-field push | 2N |

The compiler still TYPECHECKS at compile time —
the array's row-size information is preserved
in the type, so the callee can compute correct
offsets. But the runtime representation is just
the pointer to the first byte.

**Bitwise compound assignment encoding**:
| Op | ModR/M | Reg w/ imm16 |
|----|--------|---------------|
| `\|=` | `81 /1` | `81 ce xx xx` (SI), `81 c8 xx xx` (AX), etc. |
| `&=` | `81 /4` | `81 e6 xx xx` (SI), etc. |
| `^=` | `81 /6` | `81 f6 xx xx` (SI), etc. |
| `+=` | `81 /0` | `81 c6 xx xx` (SI) |
| `-=` | `81 /5` | `81 ee xx xx` (SI) |

(For small imm8, the `83 /N imm8` 3-byte form may
be used instead.)

For the Rust reimplementation:
- Comma op: emit subexpressions sequentially;
  keep value of last.
- Compound assignment to reg + imm: emit `81 /N`
  form.
- Array args: decay to pointer; track type for
  offset computation in callee.

## `char *arr[]` = N pointers w/ FIXUPP; non-static aggregate init = N_SCOPY@ from _DATA at fn entry

Fixtures `2231` (array of string pointers),
`2232` (2D array on stack), `2233` (string-init
non-static char arr) cover initialization
mechanisms.

- `2231` (**array of string pointers**): 
  ```
  ; _DATA layout:
  06 00 0c 00 11 00        ; 3 near pointers (FIXUPP'd)
  61 6c 70 68 61 00        ; "alpha\0" at offset 6
  62 65 74 61 00            ; "beta\0" at offset 12
  67 61 6d 6d 61 00        ; "gamma\0" at offset 17
  ```
  Indexing `names[i][j]`:
  ```
  mov bx, [names + i*2]      ; load ptr
  mov al, [bx + j]           ; deref byte
  ```
- `2232` (**non-static 2D array on stack**):
  ```
  ; In _DATA: 1,2,3,4,5,6,7,8,9 (9 ints, 18 bytes)
  
  ; In main:
  push ss / lea ax, [m] / push ax        ; dest = stack slot
  push ds / mov ax, 0 / push ax           ; src = _DATA init
  mov cx, 18 (sizeof)                     ; bytes to copy
  call N_SCOPY@
  ```
  Indexing `m[i][j]`: compile-time offset = `i *
  cols * sizeof + j * sizeof`. For 3×3 ints,
  `m[1][1]` at offset 8 from base.
- `2233` (**non-static `char buf[16] = "hello"`**):
  same N_SCOPY@ mechanism — `"hello\0..."` (16
  bytes including trailing zeros) in `_DATA`,
  copied to stack buf at fn entry:
  ```
  push ss / lea ax, [buf] / push
  push ds / push 0 (offset of "hello\0...")
  mov cx, 16
  call N_SCOPY@
  ```

**Non-static aggregate initialization summary**:
| Initializer | Mechanism |
|-------------|-----------|
| Scalar: `int x = 42` | `mov [x], 42` direct store at fn entry |
| Array: `int a[3] = {...}` | N_SCOPY@ from _DATA template |
| String: `char s[N] = "..."` | N_SCOPY@ (template padded to N bytes) |
| 2D array: `int m[3][3] = {...}` | N_SCOPY@ (row-major flat) |
| Struct: `struct S s = {...}` | N_SCOPY@ from _DATA template |
| Aggregate of pointers | N_SCOPY@ (template has FIXUPP'd ptrs) |

**Static vs non-static**:
- Static: init data lives in `_DATA` directly; no
  copy needed (the variable IS the _DATA slot)
- Non-static: init data lives in `_DATA` as a
  template; N_SCOPY@ copies it to the stack slot
  at function entry

For the Rust reimplementation:
- Track aggregate initializers; emit _DATA
  template + N_SCOPY@ call at fn entry for non-
  static.
- Multidim arrays: compute row-major layout,
  pad strings to declared size.

## `continue` in for = jmp to update; nested break = innermost end label only; `goto` = unconditional jmp

Fixtures `2228` (continue), `2229` (nested break),
`2230` (goto) cover the remaining control-flow
non-locals.

- `2228` (**`continue` in for-loop**): jumps to
  the **update** label (between body and test),
  NOT the test directly:
  ```
  ; for (i=0; i<10; i++) { if (i&1) continue; s+=i; }
  body:
    test ax, 1
    je not_odd
    jmp continue_lbl    ; <-- continue
  not_odd:
    add s, i
  continue_lbl:           ; update slot
    inc i
  test:
    cmp i, 10
    jl body
  ```
  So for is unique in having a separate
  continue-target. while/do-while continue jumps
  directly to the test.
- `2229` (**nested loop break, inner only**):
  each loop has its own end_of_loop label; break
  jumps to the **innermost** enclosing one:
  ```
  outer_body:
    inner_body:
      cmp j, 2
      jl skip
      jmp inner_end          ; break inner
    skip:
      ...
    inner_update / inner_test
    inner_end:
    outer_update / outer_test
  outer_end:
  ```
- `2230` (**`goto label`**): direct
  unconditional jmp to the label:
  ```
  ; goto done;  →  jmp done
  ; if (c) goto done;  →  cmp c / jcc-inverse skip / jmp done; skip:
  ```

**Control-flow non-locals summary**:
| Construct | Behavior |
|-----------|----------|
| `break` | `jmp innermost_loop_end` or `jmp switch_end` |
| `continue` (while/do) | `jmp test` |
| `continue` (for) | `jmp update` (separate label between body and test) |
| `goto label` | `jmp label` (direct unconditional) |
| `if (c) goto X` | `cmp c / jcc-inverse skip / jmp X / skip:` |
| `return` | `jmp fn_epilogue` (or fall through) |

**Why for needs a separate continue target**:
The for-loop's update step (`i++`) must run on
continue. In while/do, no update step exists, so
continue jumps directly to the test. The for-
specific label is the only loop-structural
difference between for and while.

For the Rust reimplementation:
- Maintain a stack of (loop_end, continue_target)
  labels for nested loops.
- break / continue emit `jmp` to the innermost
  matching label.
- Switch nests separately for break (switch_end
  label), but doesn't capture continue.
- goto: emit jmp directly to the label symbol.

## do-while = simplest loop form (no top jmp); for empty-init = jmp-test header; for empty-cond = unconditional jmp

Fixtures `2225` (do-while), `2226` (for empty
init), `2227` (for empty cond + break) cover the
remaining loop variants.

- `2225` (**do/while**): simplest loop layout —
  body first, then conditional jcc back:
  ```
  ; (init declarations done outside)
  body:
    body...
    cond_test
    jcc body
  ```
  No initial jump to test. Body runs at least
  once. For i=0..4 (5 iters): sum = 0+1+2+3+4=10.
- `2226` (**for with empty init**): standard for-
  loop layout — still has the `jmp test` at top:
  ```
  ; (init done before — empty here means no
  ;  additional init code in the loop)
  jmp test
  body:
    body
    update
  test:
    cond_test
    jcc body
  ```
  Empty init is a no-op slot, but the `jmp test`
  still emits to skip the body the first time.
- `2227` (**for with empty cond + break**): empty
  condition = always-true = unconditional `jmp
  body` at the bottom:
  ```
  body:
    body
    update
  jmp body         ; unconditional (empty cond = true)
  end_of_loop:
  ```
  Break translates to `jmp end_of_loop` — same in
  all loops.

**Loop layout summary** (final):
| Loop | Layout |
|------|--------|
| `while (c) b` | `jmp test; body: b; test: c → jcc body` |
| `do b while (c)` | `body: b; c → jcc body` (no top jmp) |
| `for (i; c; u) b` | `i; jmp test; body: b; u; test: c → jcc body` |
| `for (i; ; u) b` (empty cond) | `i; body: b; u; jmp body` (unconditional) |
| `for (; c; u) b` (empty init) | `jmp test; body: b; u; test: c → jcc body` |
| `for (;;) b` | `body: b; jmp body` |
| `break` | `jmp end_of_loop` |
| `continue` | `jmp test_or_update` |

So **while** and **for** share the same skeleton
(jmp-test-first) regardless of which clauses are
empty. The difference is just where init/update
go. **do/while** is unique in having no top jump.

For the Rust reimplementation:
- do/while: emit body THEN test.
- for/while: emit jmp-to-test at top.
- Empty cond: emit unconditional jmp instead of
  cond_test.
- break: jmp end_of_loop.

## Switch only-default = unconditional body; fall-through = linear bodies; multi-case shares one target

Fixtures `2222` (switch default-only), `2223`
(fall-through), `2224` (multi-case shared body)
cover switch-statement edge cases.

- `2222` (**switch with only default**): no cases
  to check — default body executed unconditionally:
  ```
  ; switch (x) { default: r = 99; break; }
  ; (no jcc structure, just the body)
  mov word [r], 99
  ; jmp +0 (no-op for break/end)
  ```
- `2223` (**fall-through cases**): each case body
  emitted in order; no inter-case jmp; control
  flows through:
  ```
  ; switch(x) { case 1: r+=10; case 2: r+=20; case 3: r+=30; }
  cmp ax, 1 / je case1
  cmp ax, 2 / je case2
  cmp ax, 3 / je case3
  jmp end                 ; no default — fall out
  case1: add si, 10
  case2: add si, 20       ; falls through from case1
  case3: add si, 30       ; falls through from case2
  end:
  ```
  For x=1, all three case bodies execute: r = 10+20+30 = 60.
- `2224` (**multi-case shared action**): all `case
  N:` labels for the same action point to a
  single shared target — body emitted once:
  ```
  ; switch(x) { case 1: case 2: case 3: r=100; break; default: r=0; }
  cmp ax, 1 / je shared
  cmp ax, 2 / je shared    ; same target
  cmp ax, 3 / je shared    ; same target
  jmp default              ; (no match)
  shared:
    mov si, 100
    jmp end                 ; break
  default:
    xor si, si
  end:
  ```

**Switch codegen tactics summary** (refined):
| Pattern | Tactic |
|---------|--------|
| Default only | Emit body unconditionally |
| Fall-through | Linear bodies, no inter-case jmp |
| Multi-case same action | Shared label target |
| Dense cases (≥ 4 contiguous) | Jump table |
| Sparse cases (≤ 3 or non-contiguous) | Linear cmp/je chain |
| Sparse but ≥ 4 cases | Search table |

For the Rust reimplementation:
- Default-only switch: bypass jcc; emit body.
- Multi-case same action: dedup labels.
- Fall-through: no extra jmp between case bodies.

## `(char)int` = free low-byte load; `(int)uchar` = `mov ah, 0`; `char + int` = cbw then add

Fixtures `2219` (int↔char round-trip), `2220`
(uchar→int), `2221` (char+int) finalise the
narrowing/widening table.

- `2219` (**`(char)int` then `(int)char`**):
  ```
  ; (char)n — free truncation via low-byte load:
  mov al, [n]              ; loads low byte (n at base addr)
  mov [c], al              ; byte store
  
  ; (int)c — sign-extend:
  mov al, [c]
  cbw                       ; sign-extend AL → AX
  mov [r], ax
  ```
  Round-trip preserves low byte; sign of result
  depends on bit 7 of the low byte.
- `2220` (**`(int)unsigned char` zero-extends**):
  uses `mov ah, 0` (2 bytes, vs cbw's 1 byte):
  ```
  mov al, [uc]
  mov ah, 0                ; b4 00 — zero-extend
  mov [n], ax
  ```
  For uc = 200: n = 200 (positive, preserved).
  With cbw it would be n = -56 (sign-extended).
- `2221` (**`char + int` promotion**): char is
  promoted to int first via cbw, then added:
  ```
  mov al, [c]
  cbw                       ; promote to int
  add ax, [n]
  ```

**Type-conversion summary** (final, complete):
| Operation | Mechanism | Bytes |
|-----------|-----------|-------|
| `(char)int` | Load low byte | 0-3 |
| `(int)signed char` | `cbw` (sign-ext) | 1 |
| `(int)unsigned char` | `mov ah, 0` | 2 |
| `(long)signed int` | `cwd` (sign-ext) | 1 |
| `(long)unsigned int` | `mov [hi], 0` | 5 |
| `(int)long` | Read low word | 0-3 |
| `(double)int` | spill + FILD m16 | ~8 |
| `(double)float` | FLD m32 / FSTP m64 | 8 |
| `(int)float`, `(int)double` | FLD + call N_FTOL@ | ~7 |
| `char + int`, `char op int` | cbw on char first | varies |
| `int + long` | cwd on int first | varies |
| `float + double` | FLD m32 widens (automatic) | varies |

So **promotion always widens the smaller type**
before the operation. BCC follows C's "usual
arithmetic conversions" rules.

For the Rust reimplementation:
- Track types per expression node.
- Insert promotion instructions before mixed-type
  ops based on the "usual conversions" rules.

## `x * 4` ≡ `x << 2` codegen; `unsigned x % 2` ≡ `x & 1`; `**pp` = 2 mem loads `8b 1c / 8b 07`

Fixtures `2216` (mul-pow2 vs shift), `2217` (mod-2
vs and-1), `2218` (double deref) verify expected
equivalences.

- `2216` (**`x * 4` ≡ `x << 2`**): both lower to
  `shl ax, 1 / shl ax, 1` (4 bytes). BCC's pow-2
  recognition makes them byte-identical.
- `2217` (**unsigned `x % 2` ≡ `x & 1`**): both
  lower to `and ax, 1` (`25 01 00`, 3 bytes).
  Unsigned-only — signed `% 2` would need idiv
  for correct -1 % 2 = -1 semantics.
- `2218` (**double deref `**pp`**): clean two-
  load sequence:
  ```
  mov si, pp_addr           ; load pp (outer ptr)
  mov bx, [si]              ; bx = *pp = inner ptr
  mov ax, [bx]              ; ax = **pp = value
  ```
  Total 4 bytes for the two derefs (`8b 1c / 8b
  07`).

**Codegen-equivalence summary**:
| C expression | Equivalent | Bytes |
|--------------|------------|-------|
| `x * 1` | `x` (identity-fold) | 0 (no op) |
| `x * 2^N` (N ≤ 3) | N× `shl ax, 1` | 2N |
| `x * 2^N` (N ≥ 4) | `mov cl, N / shl ax, cl` | 4 |
| `x << N` | (same as above) | (same) |
| `unsigned x % 2^N` | `and ax, (2^N - 1)` | 3 |
| `unsigned x / 2^N` | `shr` (logical) | 2N or 4 |
| `unsigned x & (2^N - 1)` | (same as %) | 3 |

So **BCC fully normalises** these idioms at parse
time — `x * 4` and `x << 2` are not just
semantically equal but emit the exact same opcode
bytes. The `*` and `<<` (and similarly `%` and
`&`) entered the same codegen path.

For the Rust reimplementation:
- Detect pow-2 multipliers; lower to shift.
- Detect pow-2-minus-1 masks; lower to AND.
- Double deref: emit two indirect loads through
  the same or different registers.

## Large model = same as medium (far code, near data via DGROUP); huge is the only model w/ far DATA

Fixtures `2213` (large fn call), `2214` (large
global int), `2215` (large global arr) clarify
what's different in large vs medium model for
basic code.

- `2213` (**large model fn call**): **byte-
  identical** to medium model — `push cs / call
  near` (`0e e8 [rel]`) intra-segment, `cb` far
  ret.
- `2214` (**large model global int**): **byte-
  identical** to medium model — near DATA access
  via DGROUP (`a1 disp16`). No segment override.
- `2215` (**large model global array**): same
  near data access pattern. No `push ds` envelope
  needed.

**Memory model differences observed** (final):
| Model | Code | Data | Code seg | Data access |
|-------|------|------|----------|-------------|
| small (-ms) | near | near | `_TEXT` | `a1 disp16` |
| compact (-mc) | near | near | `_TEXT` | `a1 disp16` |
| medium (-mm) | far | near | `<fname>_TEXT` | `a1 disp16` (DGROUP) |
| large (-ml) | far | near | `<fname>_TEXT` | `a1 disp16` (DGROUP) |
| huge (-mh) | far | **far** | `<fname>_TEXT` | `push ds / mov ds, seg / a1 / pop ds` |

So **only huge model has FAR data** in the basic
case. Compact and large in BCC 2.0 are effectively
same as small/medium respectively, as far as basic
codegen goes. Differences would only manifest with
explicit `far` data declarations or huge-data
scenarios (data > 64K).

The 5 models compress to 3 effective code-class
behaviours:
- "near code + near data": small, compact
- "far code + near data": medium, large
- "far code + far data": huge

For the Rust reimplementation:
- Treat compact = small for trivial cases; large
  = medium for trivial cases.
- Differentiating compact/large from their pairs
  requires explicit `far` data or > 64K data
  sections.

## Medium model: extern fn = full CALL FAR (`9a`); fn ptr = 4B (CS+offset); static fn = intra-seg same as global

Fixtures `2210` (medium extern fn), `2211` (medium
fn ptr), `2212` (medium static fn) characterise
medium-model function calls.

- `2210` (**medium extern fn → full CALL FAR**):
  for cross-segment extern calls, BCC emits the
  **full 5-byte CALL FAR** with FIXUPP for both
  offset and segment:
  ```
  push fmt_offset
  9a 00 00 00 00         ; CALL FAR _printf (FIXUPP for offset+segment)
  pop cx
  retf (cb)
  ```
  Contrast with intra-segment medium calls (push
  cs + call near = 4 bytes).
- `2211` (**medium fn ptr = 4 bytes**): function
  pointers in medium model are **far pointers**
  (offset + segment, 4 bytes):
  ```
  ; Construct fp:
  mov [fp.seg], cs         ; 8c 4e fe (use CS as segment)
  mov [fp.off], offset_of_dbl   ; c7 46 fc 00 00 (FIXUPP)
  
  ; Call through fp:
  ff 5e fc                 ; call far [bp-4]
                            ; ModR/M /3 = call far indirect through m32
  ```
  ModR/M `5e disp8` = /3 = call far indirect.
- `2212` (**medium static fn**): no PUBDEF, intra-
  segment `push cs / call near`. Same as global
  default fn in medium model. The `static`
  modifier only affects symbol visibility, not
  the call form.

**Medium-model call forms (complete)**:
| Call type | Form | Bytes |
|-----------|------|-------|
| Same-CS intra-segment (default or static) | `0e e8 [rel]` (push cs + call near) | 4 |
| Cross-segment extern | `9a [off] [seg]` (CALL FAR, FIXUPP) | 5 |
| Through near fn ptr (forced) | `ff /2 [m]` | varies |
| Through far fn ptr (default in medium) | `ff /3 [m]` | varies |

**Far-pointer encoding (medium model)**:
- fn pointer = 4 bytes (segment + offset, little-endian)
- Constructed: `mov [hi], cs` + `mov [lo], offset_fixup`
- Dereferenced: `ff /3` (call far indirect)

For the Rust reimplementation:
- Medium model extern: emit `9a 00 00 00 00` with
  FIXUPP for both offset and segment.
- Medium fn pointers: 4 bytes, construct via cs
  + offset FIXUPP.
- Calls through fn ptr: `ff /3` for far, `ff /2`
  for near.

## Large struct return = hidden ptr arg + N_SCOPY@; struct arr index = i × stride; struct fn-ptr call via `ff /2`

Fixtures `2207` (struct return >4B), `2208`
(struct array iteration), `2209` (struct with fn
ptr field) complete the struct survey.

- `2207` (**large struct return via N_SCOPY@**):
  caller passes a **hidden pointer arg** to its
  receiving slot before the explicit args.
  Callee:
  1. Builds the struct locally on stack
  2. Calls N_SCOPY@ to copy local → caller's slot
  
  Calling sequence:
  ```
  ; Caller:
  push ss / push offset(bg)       ; hidden dest ptr (caller's slot)
  push X                            ; explicit args
  call _make_big
  add sp, N                         ; cleanup explicit args
  
  ; In callee make_big (simplified):
  ; ... build local b ...
  push word [bp+6] / push word [bp+4]    ; dest segment+offset (hidden arg)
  push ss / push offset(local_b)          ; src
  mov cx, 8                                ; sizeof
  call N_SCOPY@                            ; helper does the copy
  mov ax, [bp+4]                           ; return slot offset
  ret
  ```
- `2208` (**indexed struct array access**):
  `pts[i].field` = `*((char*)pts + i*sizeof(struct) + field_offset)`. Index multiplied by struct
  size via unrolled shifts:
  ```
  mov ax, i / shl ax, 1 / shl ax, 1     ; ax = i * 4 (= sizeof struct P)
  mov bx, ax
  add bx, pts                            ; bx = &pts[i]
  mov ax, [bx]                           ; pts[i].x
  add ax, [bx+2]                         ; + pts[i].y
  ```
- `2209` (**struct with fn-ptr field**): indirect
  call via memory operand:
  ```
  ff 16 00 00          ; call near [ops[0].fn] (FIXUPP for ops[0])
  ; result in AX
  ```
  ModR/M `16 disp16` = call near indirect through
  m16 (direct addressing with FIXUPP).

**Struct-return ABI (complete)**:
| Struct size | Mechanism |
|-------------|-----------|
| 1-2 bytes (int-sized) | Return in AX |
| 3-4 bytes | Return in DX:AX (high:low) |
| > 4 bytes | Hidden ptr arg + N_SCOPY@ at end of callee |

**Helper symbols (final catalogue)**:
| Helper | Purpose |
|--------|---------|
| `N_LXMUL@` | long signed/unsigned multiply |
| `N_LDIV@` | long signed divide |
| `N_LMOD@` | long signed modulo |
| `N_LUDIV@` | long unsigned divide |
| `N_LUMOD@` | long unsigned modulo |
| `N_LXLSH@` | long left shift (≥ 2) |
| `N_LXRSH@` | long signed right shift |
| `N_LXURSH@` | long unsigned right shift (probable) |
| `N_FTOL@` | float/double → long |
| `N_SPUSH@` | push struct on stack (arg passing) |
| `N_SCOPY@` | copy struct memory-to-memory (return) |
| `N_OVERFLOW@` | stack overflow handler (-N flag) |

For the Rust reimplementation:
- Struct return > 4B: add hidden ptr arg first;
  callee calls N_SCOPY@ to fill caller's slot.
- Indexed struct: emit `i * sizeof(struct)` via
  shifts, then add base + field offset.
- Fn-ptr in struct: emit `ff /2 [m]` indirect call.

## Small struct returns/args in DX:AX or stack-push; large structs use N_SPUSH@ helper

Fixtures `2204` (struct return), `2205` (struct
arg ≤ 4B), `2206` (struct arg > 4B) pin the
struct-by-value ABI.

- `2204` (**small struct return ≤ 4 bytes**):
  callee builds the struct in memory, then loads
  fields into DX:AX:
  ```
  ; In make_pt:
  ; build struct on stack
  mov dx, [p.y]              ; high half = field 1
  mov ax, [p.x]              ; low half = field 0
  ret
  
  ; Caller:
  call _make_pt
  mov [p.y], dx              ; store fields back
  mov [p.x], ax
  ```
  Same convention as long return (DX:AX).
- `2205` (**small struct arg ≤ 4 bytes**): pushed
  as whole struct (field by field), one word per
  field:
  ```
  push word [pt.y]            ; high-offset field first
  push word [pt.x]            ; low-offset field second
  call _sum_pt
  pop cx / pop cx            ; cleanup 4 bytes
  ```
  Push order ensures memory layout matches:
  after pushes, callee sees [bp+4]=x, [bp+6]=y.
- `2206` (**large struct arg > 4 bytes via
  N_SPUSH@**): for struct sizes > 4 bytes, BCC
  calls the **N_SPUSH@** helper:
  ```
  lea ax, [bg]                ; AX = struct offset
  mov dx, ss                  ; DX = struct segment (SS for stack vars)
  mov cx, 8                   ; CX = byte count
  call N_SPUSH@               ; helper pushes struct
  call _sum_big
  add sp, 8                   ; cleanup struct size
  ```
  N_SPUSH@ signature:
  - In: DX:AX = source ptr (segment:offset), CX = bytes
  - Effect: pushes the struct's bytes onto the caller's stack

**Struct-by-value ABI**:
| Direction | Size | Mechanism |
|-----------|------|-----------|
| Return ≤ 4B | int (2B) or "long" (4B) | DX:AX |
| Return > 4B | (not yet probed — likely caller-provided slot) | N_SCOPY@? |
| Arg ≤ 4B | 1-2 push words | direct push per field |
| Arg > 4B | N bytes pushed | N_SPUSH@ helper |

For the Rust reimplementation:
- Struct return ≤ 4B: emit fields → DX:AX before
  ret.
- Struct return > 4B: investigate the caller-
  slot convention.
- Struct arg ≤ 4B: emit per-field pushes (high-
  offset field first).
- Struct arg > 4B: emit N_SPUSH@ helper call.

## Multi-arg printf R-to-L w/ natural sizes; `while(i--)` test-old/body-new; strcmp loop = nested byte cmps

Fixtures `2201` (printf mixed types), `2202`
(while postdec), `2203` (strcmp-like loop) cover
multi-arg push and loop idioms.

- `2201` (**printf with int, long, double mix**):
  args pushed R-to-L in natural sizes:
  ```
  ; Source: printf("%d %ld %f\n", i, l, d);
  
  ; Push d (rightmost):
  FLD m64 [d] / add sp, -8 / FSTP m64 [sp]    ; 8 bytes
  
  ; Push l (middle):
  push word [l.hi]                              ; HIGH first
  push word [l.lo]                              ; LOW second (lower stack addr)
  
  ; Push i:
  push word [i]                                  ; 2 bytes
  
  ; Push fmt addr:
  mov ax, 8 / push ax                            ; 2 bytes
  
  call _printf
  add sp, 0x10                                   ; cleanup 16 = 8+4+2+2
  ```
- `2202` (**`while (i--)` confirmed**): test uses
  OLD i; body uses NEW i (post-decrement):
  ```
  jmp test
  body:
    add di, si              ; sum += i (using NEW i)
  test:
    mov ax, si              ; capture OLD i
    dec si                  ; i--
    or ax, ax               ; test OLD
    jne body
  ```
  For i=10, body runs 10 times with NEW i = 9,8,...,0.
  Sum = 0+1+...+9 = 45.
- `2203` (**strcmp-like loop**): `while (*a &&
  *b && *a == *b) { a++; b++; }` lowers to a
  nested byte-test chain:
  ```
  loop:
    cmp byte [si], 0          ; test *a
    je L_exit
    cmp byte [di], 0          ; test *b
    je L_exit
    mov al, [si]              ; *a
    cmp al, [di]              ; *a == *b ?
    jne L_exit
    inc si / inc di           ; a++, b++ (post-cond)
  L_exit:
  ```

**Multi-arg push order — final**:
For `f(a, b, c)` cdecl with types (T1, T2, T3):
1. Push c (size of T3 first)
2. Push b (size of T2)
3. Push a (size of T1)
4. Call f
5. Cleanup `add sp, total_bytes` (or `pop cx × N` if ≤ 4 bytes)

Each long is pushed as `hi / lo` (so lo ends at lower offset).
Each double via `add sp,-8 / FSTP m64`.
Each int/ptr via single push word.

For the Rust reimplementation:
- Push args in source-right-to-source-left.
- For each arg, emit the per-type push sequence.
- Track total bytes for cleanup.

## Printf float varargs promotes to double (FLD m32 + FSTP m64); char promotes via cbw; strcpy R-to-L ptrs

Fixtures `2198` (printf float promoted), `2199`
(printf char promoted), `2200` (strcpy call)
confirm "default argument promotions" for varargs
and external calls.

- `2198` (**printf float → double promotion**):
  ```
  FLD m32 [f]            ; load float (FPU widens to 80-bit)
  add sp, -8              ; allocate 8B for double
  FSTP m64 [sp]          ; store as DOUBLE (not m32!)
  push fmt
  call _printf
  add sp, 10              ; cleanup 8B + 2B
  ```
  Per C standard, **float is promoted to double**
  for varargs. The FPU's internal 80-bit precision
  makes this lossless.
- `2199` (**printf char → int promotion**):
  ```
  mov al, [c]
  cbw                     ; SIGN-extend to AX (char is signed)
  push ax                  ; push as int (2 bytes)
  push fmt
  call _printf
  pop cx / pop cx        ; cleanup 4 bytes
  ```
  Per C standard, **char/short are promoted to
  int** for varargs. `cbw` for signed char; would
  be `mov ah, 0` for unsigned char.
- `2200` (**strcpy(dest, src) call**): standard
  cdecl R-to-L push:
  ```
  push [src_addr]         ; "hello"
  push [dest_addr]         ; buf
  call _strcpy
  pop cx / pop cx         ; cleanup 4B (2 ptrs)
  ```

**Default argument promotions** (for varargs/no-
prototype calls):
| Source type | Promoted to | Mechanism |
|-------------|-------------|-----------|
| `char` (signed) | `int` | `cbw` |
| `unsigned char` | `int` | `mov ah, 0` |
| `short` | `int` | (already int width) |
| `float` | `double` | FLD m32 + FSTP m64 |
| `int`, `long`, `double`, ptr | (no promotion) | direct push |
| `struct` | (passed as-is) | N_SPUSH@ for >4B |

These promotions are why varargs functions can
safely assume args at minimum int-width / double-
precision-float in the stack frame.

For the Rust reimplementation:
- Varargs/no-prototype calls: emit promotion code
  for char (cbw), unsigned char (mov ah, 0),
  float (FLD/FSTP m64).
- Multi-arg external calls: push R-to-L.

## Printf double arg = add sp,-8 + FSTP m64; long arg = push hi/lo; string arg = push addr

Fixtures `2195` (printf double), `2196` (printf
string), `2197` (printf long) pin the varargs
arg-push conventions per type.

- `2195` (**printf with `double` arg**):
  ```
  FLD m64 [3.14]            ; load double (3.14 not exact-single, 8B)
  add sp, -8                 ; allocate 8 bytes for double arg
  FSTP m64 [sp]             ; store double on stack
  push ax                    ; push fmt addr
  WAIT + call _printf
  add sp, 10                 ; cleanup 8B (double) + 2B (string)
  ```
  Cleanup = `add sp, 10` (8 + 2 = 10 bytes).
- `2196` (**printf with string arg**): just
  pushes the 2-byte near pointer:
  ```
  push [hello_addr]          ; 2 bytes
  push [fmt_addr]            ; 2 bytes
  call _printf
  pop cx / pop cx           ; cleanup 4 bytes (falls into pop-cx threshold)
  ```
- `2197` (**printf with `long` arg**):
  ```
  push word [n.hi]           ; high half pushed FIRST
  push word [n.lo]           ; low half pushed SECOND
  push [fmt_addr]
  call _printf
  add sp, 6                  ; cleanup 4B long + 2B string = 6B
  ```
  Push order: hi then lo. On stack (which grows
  down), this puts lo at lower addr (sp+0), hi
  at higher (sp+2) — the standard little-endian
  long layout that printf expects.

**Varargs arg-push sizes by type**:
| Type | Bytes pushed | How |
|------|--------------|-----|
| `char`, `short` | 2 (promoted to int) | (single push) |
| `int`, near ptr | 2 | push word |
| `long`, `unsigned long` | 4 | push hi / push lo |
| `float` | 4 (promoted to double) | add sp,-4 + FSTP m32 |
| `double` | 8 | add sp,-8 + FSTP m64 |
| Struct | sizeof(struct) | N_SPUSH@ helper |
| String literal | 2 | push offset |

Note: per C standard, `float` arg through varargs
is **promoted to `double`** (so 8 bytes total).

**Cleanup pattern reminder**:
- 0 bytes: no cleanup
- 2 bytes: pop cx
- 4 bytes: pop cx × 2
- 6+ bytes: add sp, N

For the Rust reimplementation:
- Printf-style varargs: push args R-to-L per type
  with the right size. Cleanup based on total
  bytes pushed.
- Float-through-varargs: promote to double before
  pushing.

## `int * double` = FILD m16 + FMUL m64; double == 0.0 = FLDZ + FCOMPP; printf varargs = R-to-L + caller cleanup

Fixtures `2192` (int × double), `2193` (double ==
0.0), `2194` (printf with 3 args) cover three
mixed/varargs idioms.

- `2192` (**`int * double` promotion**): int gets
  loaded into FPU via **FILD m16** (the 16-bit
  integer load), then FMUL m64 with the double:
  ```
  mov [tmp], i              ; spill int to memory
  9b df 46 ec               ; FILD m16 (load 16-bit int → FPU as float)
  9b dc 4e f6               ; FMUL m64 [d]
  9b dd 5e ee               ; FSTP m64 [r]
  ```
  FILD opcodes by integer width:
  - `df /0` = FILD m16 (16-bit)
  - `db /0` = FILD m32 (32-bit)
  - `df /5` = FILD m64 (64-bit)
- `2193` (**`double == 0.0` via FLDZ + FCOMPP**):
  same pattern as float-vs-zero (fixture 2151):
  ```
  9b dd 46 f8               ; FLD m64 [d]
  9b d9 ee                  ; FLDZ (load 0.0 to FPU)
  9b de d9                  ; FCOMPP (compare and pop both)
  9b dd 7e f6               ; FSTSW m16
  90 / 9b 8b 46 f6 / 9e     ; status → AX → flags
  jne L_false               ; (for == test)
  ```
  Single FLDZ avoids needing a 0.0 constant in
  `_DATA`. Works the same for both float and
  double.
- `2194` (**`printf(fmt, a, b, c)` varargs**):
  ```
  push 3 / push 2 / push 1  ; R-to-L per cdecl
  mov ax, 0 / push ax        ; push fmt string addr (FIXUPP)
  call _printf               ; e8 [disp] (FIXUPP)
  add sp, 8                  ; caller cleanup 4 args × 2B
  ```
  
  String "a=%d b=%d c=%d\n\0" stored in `_DATA`
  (16 bytes).

**Variadic call summary**:
| Aspect | Detail |
|--------|--------|
| Arg push order | R-to-L (cdecl convention) |
| Cleanup | Caller — `add sp, N*2` for N word args |
| Varargs receiver | Reads via pointer math from `&first_named_arg` |
| `va_list` access | (not yet probed — likely `&...` semantics) |

For the Rust reimplementation:
- Mixed int + double: spill int, emit FILD m16 +
  FMUL m64.
- Use FLDZ/FLD1 for float/double 0.0/1.0 consts.
- Variadic calls: push R-to-L; caller cleanup.

## `(long)int` = cwd sign-ext; `(long)unsigned int` = zero store; long+int promotes via cwd

Fixtures `2189` (signed int → long), `2190`
(unsigned int → long), `2191` (long + int mixed)
cover long-promotion mechanisms.

- `2189` (**`(long)signed int` = cwd**): single
  byte sign-extension:
  ```
  mov ax, [i]
  cwd                  ; 99 — sign-extend AX into DX:AX
  mov [l.hi], dx
  mov [l.lo], ax
  ```
  CWD copies AX's sign bit (bit 15) to all bits
  of DX. For i = -5 (0xFFFB), DX becomes 0xFFFF.
- `2190` (**`(long)unsigned int` = zero store**):
  zero-extension via direct mov of 0:
  ```
  mov ax, [u]
  mov word [l.hi], 0     ; c7 46 fc 00 00 — direct zero store
  mov [l.lo], ax
  ```
  Note: BCC does NOT use `xor dx, dx` (2 bytes) +
  store dx. Instead emits the 5-byte direct mov
  imm16 = 0. Possibly because the destination is
  already memory.
- `2191` (**long + int mixed**): the int is
  **promoted to long first** (via cwd), then the
  long+long inline add idiom runs:
  ```
  mov ax, [i]
  cwd                   ; promote i to (DX:AX)
  mov bx, [l.hi]
  mov cx, [l.lo]
  add cx, ax            ; lo + lo
  adc bx, dx            ; hi + hi + carry
  ```
  So 4 registers used (AX, DX, BX, CX) to hold
  both 32-bit operands.

**Long promotion patterns**:
| Source | Mechanism | Bytes |
|--------|-----------|-------|
| `signed int` → `long` | `cwd` | 1 |
| `unsigned int` → `long` | `mov word [hi], 0` | 5 (or alternative: `xor dx, dx` 2B + store) |
| `signed char` → `long` | `cbw` (→ AX) then `cwd` (→ DX:AX) | 2 |
| `unsigned char` → `long` | `mov ah, 0` then `mov word [hi], 0` | 7 |
| `long` → `int` | Take low word directly | 0 (free truncation) |
| `long` → `char` | Take low byte directly | 0 (free truncation) |

For mixed arith with promotion, the smaller type
is widened first, then both run through the
larger type's arithmetic.

For the Rust reimplementation:
- (long)int signed: emit `cwd` after the int load.
- (long)int unsigned: emit direct zero store to
  the high half.
- Mixed long+int: promote the int first.

## `~long` = `not/not` inline (4B); `long == 0` uses `or low, high` zero-test; same for `if (long)`

Fixtures `2186` (~long), `2187` (long == 0), `2188`
(`if (long)`) finish the long-unary survey.

- `2186` (**`~long` bitwise NOT**): inline via two
  `not` instructions:
  ```
  ax = a.hi / dx = a.lo
  not dx               ; f7 d2 — flip low
  not ax               ; f7 d0 — flip high
  ```
  No carry propagation needed (bitwise op). 4
  bytes total.
- `2187` (**`long == 0` via OR**): single `or`
  combines both halves into a zero-test:
  ```
  mov ax, [a.lo]
  or ax, [a.hi]        ; 0b 46 fe — ZF iff both halves zero
  jne L_skip           ; (skip the body — a != 0)
  ; body: return 0
  ```
  Clever optimisation: instead of two cmp+jne,
  single `or` sets ZF iff the combined long is
  zero.
- `2188` (**`if (long_val)` as condition**): same
  OR-pattern but with `je` (skip if zero):
  ```
  mov ax, [a.lo]
  or ax, [a.hi]
  je L_false           ; 74 05 — skip body if zero
  ```

**`long` zero-test pattern**:
| Source | jcc | Sense |
|--------|-----|-------|
| `if (a)` or `if (a != 0)` | `je` (skip if zero) | execute body if non-zero |
| `if (a == 0)` | `jne` (skip if non-zero) | execute body if zero |

Matches the int register-form pattern (`or reg,
reg`) we saw earlier. The long version just uses
a memory operand for the second half.

**Long unary ops** (complete):
| Op | Form | Bytes |
|----|------|-------|
| `-a` | `neg ax / neg dx / sbb ax, 0` | 5 |
| `~a` | `not dx / not ax` | 4 |
| `(unsigned long)a` | (no-op, type-only) | 0 |
| `(long)int` | sign-extend via cwd | 1-3 |
| `if (a)` / `if (!a)` / `a == 0` / `a != 0` | `or low, high` + jcc | 5-7 |

For the Rust reimplementation:
- `~long`: emit `not dx / not ax` (no helper).
- `long == 0` test: emit `or low, high` then
  appropriate jcc.

## Signed `long >> 1` = `sar/rcr` inline; ulong % = N_LUMOD@; `-long` = `neg/neg/sbb` (3-instruction idiom)

Fixtures `2183` (signed long >> 1), `2184` (ulong
%), `2185` (long unary -) complete the long-
operator catalogue.

- `2183` (**signed `long >> 1` inline**): uses
  **SAR** (arithmetic right shift) on the high
  half:
  ```
  ax = a.hi / dx = a.lo
  sar ax, 1            ; d1 f8 — ARITHMETIC right (preserves sign)
  rcr dx, 1            ; d1 da — rotate low through carry
  ```
  For -1 >> 1, SAR keeps the sign bit set → result
  stays -1. (Contrast: SHR would give a huge
  unsigned positive number.)
- `2184` (**ulong %**): uses **N_LUMOD@** helper
  (distinct from signed N_LMOD@). Same calling
  convention (stack-push, DX:AX return).
- `2185` (**unary `-` on long**): 5-byte inline
  idiom:
  ```
  ax = a.hi / dx = a.lo
  neg ax              ; f7 d8 — flip high
  neg dx              ; f7 da — flip low (CF set if dx was nonzero)
  sbb ax, 0           ; 1d 00 00 — adjust high by borrow from low
  ```
  Beautiful: the SBB propagates the borrow from
  low's negation into the high half. Total 5
  bytes (2+2+3).

**Long-shift inline cases (complete)**:
| Operation | Form |
|-----------|------|
| `long << 1` (signed/unsigned) | `shl dx, 1 / rcl ax, 1` (4B) |
| `unsigned long >> 1` | `shr ax, 1 / rcr dx, 1` (4B) |
| `signed long >> 1` | `sar ax, 1 / rcr dx, 1` (4B) |
| `-long` (unary minus) | `neg ax / neg dx / sbb ax, 0` (5B) |
| `~long` (bitwise NOT) | `not ax / not dx` (4B, probable) |
| Long shift by N ≥ 2 or var | Helper (N_LXLSH@, etc.) |

**Long helper catalogue (final, confirmed)**:
| Helper | Op | Sign |
|--------|-----|------|
| `N_LXMUL@` | × | both share |
| `N_LDIV@` | / | signed |
| `N_LMOD@` | % | signed |
| `N_LUDIV@` | / | unsigned |
| `N_LUMOD@` | % | unsigned |
| `N_LXLSH@` | << | both share |
| `N_LXRSH@` | >> | signed (probable) |
| `N_LXURSH@` | >> | unsigned (probable) |
| `N_FTOL@` | float→long | both |

For the Rust reimplementation:
- Signed long >> 1: emit `sar/rcr`.
- Ulong %: emit N_LUMOD@ helper call.
- Long unary minus: emit `neg/neg/sbb 0` 5-byte
  idiom.

## Ulong mul = N_LXMUL@ (shared signed); ulong div = N_LUDIV@ separate; ulong >> 1 inline `shr/rcr`

Fixtures `2180` (ulong mul), `2181` (ulong div),
`2182` (ulong >> 1) complete the long-helper
characterization.

- `2180` (**unsigned long × → N_LXMUL@**): **same**
  helper as signed long multiply. Multiplication's
  low 32 bits are bit-pattern-identical for signed
  vs unsigned — BCC discards the high half anyway,
  so a single helper suffices.
- `2181` (**unsigned long / → N_LUDIV@**):
  **separate** helper from signed `N_LDIV@`. Same
  calling convention (stack-pushed args), but the
  routine handles unsigned-specific overflow and
  rounding correctly.
- `2182` (**unsigned `long >> 1` inline**):
  ```
  ax = a.hi / dx = a.lo
  shr ax, 1            ; d1 e8 — LOGICAL right shift on high
  rcr dx, 1            ; d1 da — rotate low through carry
  ; result: ax:dx = a >> 1 (logical)
  ```
  Uses **SHR** (logical, fills with 0) on the high
  half. Signed `long >> 1` would use **SAR**
  (arithmetic, preserves sign bit) instead.

**Long helper symbols catalogue** (confirmed):
| Helper | Op | Signedness | Confirmed by fixture |
|--------|-----|-----------|------------------------|
| `N_LXMUL@` | × | both | 2170 (signed), 2180 (unsigned) |
| `N_LDIV@` | / | signed | 2171 |
| `N_LMOD@` | % | signed | 2179 |
| `N_LUDIV@` | / | unsigned | 2181 (confirms guess) |
| `N_LUMOD@` | % | unsigned | (probable, not yet probed) |
| `N_LXLSH@` | << | both | 2172, 2177, 2178 |
| `N_LXRSH@` | >> | signed | (probable) |
| `N_LXURSH@` | >> | unsigned | (probable) |
| `N_FTOL@` | float→long | both | 2132, others |

**Long-shift inline cases** (final):
| Shift | Form |
|-------|------|
| `<< 1` signed/unsigned | `shl dx, 1 / rcl ax, 1` (4 bytes) |
| `>> 1` UNSIGNED | `shr ax, 1 / rcr dx, 1` (4 bytes) |
| `>> 1` SIGNED | `sar ax, 1 / rcr dx, 1` (4 bytes) [probable] |
| `<< N`, `>> N` (N ≥ 2) | N_LXLSH@ / N_LXRSH@ / N_LXURSH@ |

For the Rust reimplementation:
- Ulong mul: same N_LXMUL@ as signed.
- Ulong div/mod: N_LUDIV@/N_LUMOD@ (distinct from signed).
- N=1 long shifts: inline shl/rcl, shr/rcr, sar/rcr per signedness.

## Long shift threshold: N=1 inline (shl/rcl), N≥2 helper; `long %` uses separate N_LMOD@

Fixtures `2177` (long << 3), `2178` (long << 4),
`2179` (long %) refine long shift inlining and
introduce the N_LMOD@ helper.

- `2177` (**`long << 3` uses N_LXLSH@**): NOT
  inlined! BCC emits the helper call:
  ```
  mov dx, [a.hi] / mov ax, [a.lo]
  mov cl, 3                  ; b1 03
  call N_LXLSH@              ; e8 [disp]
  ```
- `2178` (**`long << 4` uses N_LXLSH@**): identical
  to 2177 except CL = 4.
- `2179` (**`long % long` uses N_LMOD@**): separate
  helper from N_LDIV@. Same calling convention
  (stack-pushed args, DX:AX result):
  ```
  push b.hi / push b.lo / push a.hi / push a.lo
  call N_LMOD@               ; e8 [disp]
  ; result: DX:AX = a mod b
  ```

**Refined long-shift threshold**:
- N == 1: **inline** `shl dx, 1 / rcl ax, 1` (4
  bytes)
- N ≥ 2 (any constant or variable): **N_LXLSH@**
  helper (~10 bytes including setup)

So only `<< 1` gets the inline treatment because
the 8086 has the special 1-byte-shift form `shl
reg, 1` (without needing CL or immediate). For
N ≥ 2 the helper is preferred regardless of byte
count, presumably because:
- N=2: 8 bytes unrolled (2× shl/rcl) vs ~10 helper —
  near tie, BCC picks helper for consistency
- N=3+: helper clearly cheaper

**Long helper symbols** (complete list so far):
| Helper | Purpose |
|--------|---------|
| `N_LXMUL@` | long signed multiply |
| `N_LDIV@` | long signed divide (quotient) |
| `N_LMOD@` | long signed modulo (remainder) |
| `N_LXLSH@` | long left shift |
| `N_LXRSH@` | long right shift (signed) |
| `N_LXURSH@` | long right shift (unsigned) — guess |
| `N_LUMUL@` | long unsigned multiply — guess |
| `N_LUDIV@` | long unsigned divide — guess |
| `N_LUMOD@` | long unsigned modulo — guess |
| `N_FTOL@` | float→long |
| `N_OVERFLOW@` | stack overflow |
| `N_SCOPY@` | struct copy |
| `N_SPUSH@` | struct push |

For the Rust reimplementation:
- `long << 1` inline; all other long shifts via
  N_LXLSH@ (signed) or N_LXURSH@ (unsigned).
- `long %` via N_LMOD@ helper.

## `long ==` inline (hi+lo cmp); `long <` = signed-hi + unsigned-lo branch; `long << 1` = `shl/rcl` inline

Fixtures `2174` (long ==), `2175` (long <), `2176`
(long << const) refine long comparison and shift
inlining.

- `2174` (**`long ==` inline**): two-step compare:
  ```
  ax = a.hi / dx = a.lo
  cmp ax, [b.hi]
  jne L_false           ; high halves differ → unequal
  cmp dx, [b.lo]
  jne L_false           ; low halves differ → unequal
  ; else equal
  ```
- `2175` (**`long <` signed inline**): three-stage
  comparison handling sign correctly:
  ```
  ax = a.hi / dx = a.lo
  cmp ax, [b.hi]       ; compare HIGH halves (SIGNED)
  jg L_false            ; a > b (signed) → false
  jl L_true             ; a < b (signed) → true
  cmp dx, [b.lo]       ; high halves equal; compare LOW (UNSIGNED)
  jae L_false           ; a.lo >= b.lo → false
  ; else true
  ```
  Classic signed-32 comparison: high half compared
  as signed (because sign bit lives there), low
  half as unsigned (because it has no sign bit).
- `2176` (**`long << 1` inline**): for small const
  shifts, NO helper call:
  ```
  ax = a.hi / dx = a.lo
  shl dx, 1             ; shift low half, top bit → CF
  rcl ax, 1             ; rotate high through carry
  ; result: ax:dx = a << 1 (AX=hi, DX=lo)
  ```
  Beautiful 8086 trick: shift+rotate-through-carry
  propagates the top bit of low into bottom of
  high. Likely unrolled for N ≤ 3, helper for
  N ≥ 4.

**Long register-allocation note**: BCC uses
different conventions in different contexts:
- **Inline shift**: AX = high, DX = low (so
  `shl dx, 1 / rcl ax, 1` works)
- **N_LXMUL@ helper**: DX = high, AX = low (in
  arg+result, standard "DX:AX")
- **Inline cmp**: AX = high, DX = low (since cmp
  is order-independent for `==`)
- **Inline add/sub**: AX = low, DX = high (so
  `add ax, [b.lo] / adc dx, [b.hi]` works)

Inconsistent! The convention depends on the
operation's instruction semantics. Track this
per-op when generating code.

**Long inline-vs-helper summary** (updated):
| Op | Inline form | Helper |
|----|-------------|--------|
| `==`, `!=` | hi-cmp+jne, lo-cmp+jne | none |
| `<`, `<=`, `>`, `>=` (signed) | signed hi-cmp + unsigned lo-cmp | none |
| `+`, `-` | `add/adc`, `sub/sbb` | none |
| `&`, `|`, `^` | 2× bitwise | none |
| `<< 1`, `<< 2`, `<< 3` | `shl/rcl` × N | none |
| `<< var` or `<< ≥4` | (HELPER) | `N_LXLSH@` |
| `*` | (HELPER) | `N_LXMUL@` |
| `/`, `%` | (HELPER) | `N_LDIV@` |

For the Rust reimplementation:
- Long comparisons: emit hi-first / lo-second.
- Signed long compare: signed jcc on hi, unsigned
  on lo.
- Small const shifts: emit `shl/rcl` × N.
- Track per-op register convention (hi/lo).

## long/ = N_LDIV@ via stack-push; long shift = N_LXLSH@ in regs; long+ inline `add/adc`

Fixtures `2171` (long div), `2172` (long shift),
`2173` (long add) characterise long-arithmetic
helper conventions and inline ops.

- `2171` (**long / long → N_LDIV@**): pushes
  args on stack, then calls:
  ```
  push word [b.hi] / push word [b.lo]     ; divisor (hi, lo)
  push word [a.hi] / push word [a.lo]     ; dividend (hi, lo)
  call N_LDIV@                             ; e8 [disp]
  ; result in DX:AX (quotient)
  ```
  **Different calling convention** from N_LXMUL@:
  N_LDIV@ uses stack push, N_LXMUL@ uses
  registers (CX:BX + DX:AX).
- `2172` (**long << var → N_LXLSH@**):
  ```
  mov dx, [a.hi] / mov ax, [a.lo]
  mov cl, [n]              ; shift count (single byte)
  call N_LXLSH@            ; e8 [disp]
  ; result in DX:AX (shifted long)
  ```
  Register-passed (DX:AX + CL).
- `2173` (**long + long inline, no helper**):
  ```
  mov ax, [a.lo] / mov dx, [a.hi]
  add ax, [b.lo]           ; 03 /r — adds low halves, CF set
  adc dx, [b.hi]           ; 13 /r — adds high halves WITH CARRY
  mov [r.lo], ax / mov [r.hi], dx
  ```
  ADC (`13 /r`) propagates carry from the low add.
  Total 8 bytes for the long add. No helper call.

**Long operations by category** (refined):
| Op | Inline or helper? | Helper symbol |
|----|-------------------|----------------|
| `long + long` | INLINE (`add` + `adc`) | none |
| `long - long` | INLINE (`sub` + `sbb`) | none |
| `long & long` | INLINE (`and` × 2) | none |
| `long | long` | INLINE (`or` × 2) | none |
| `long ^ long` | INLINE (`xor` × 2) | none |
| `long * long` | HELPER | `N_LXMUL@` (reg-passed) |
| `long / long` | HELPER | `N_LDIV@` (stack-push) |
| `long % long` | HELPER | `N_LDIV@` (use rem) |
| `long << count` | HELPER (if var count) | `N_LXLSH@` (reg-passed) |
| `long >> count` | HELPER | `N_LXRSH@` (signed) / `N_LXURSH@` (unsigned) |
| `long == long`, `< >` etc. | INLINE (cmp + sbb/etc.) | none |

**Helper calling-convention summary**:
| Helper | Convention |
|--------|------------|
| `N_LXMUL@`, `N_LXLSH@`, `N_LXRSH@` | Reg-passed (DX:AX, CX:BX or CL) |
| `N_LDIV@` | Stack-pushed (divisor then dividend) |
| `N_FTOL@` | FPU TOP → DX:AX |
| `N_SCOPY@`, `N_SPUSH@` | DS:SI src, ES:DI dst, CX count |

For the Rust reimplementation:
- Long mul/div/shift: emit external calls to the
  right helper with the right convention.
- Long add/sub/bitops: emit inline `add+adc` /
  `sub+sbb` / `and×2` etc.

## int*int = `imul m16` (truncates DX); unsigned cmp uses `jae`/`jb`; long*long via N_LXMUL@

Fixtures `2168` (int mul w/ overflow), `2169`
(unsigned cmp), `2170` (long mul) cover three
arithmetic-width patterns.

- `2168` (**`int * int` with overflow**): uses
  `f7 /5 [m16]` (imul m16, signed multiply):
  ```
  mov ax, a
  imul word [b]          ; f7 6e fc → DX:AX = a*b
  mov [r], ax             ; only AX stored — DX (high half) discarded
  ```
  Silent truncation to 16 bits. For 1000 × 100 =
  100000 = 0x000186A0, BCC stores AX = 0x86A0 (=
  -31072 signed) into r.
- `2169` (**unsigned `<`**): emits **unsigned jcc**
  family (`ja`, `jae`, `jb`, `jbe`) instead of
  signed (`jl`, `jle`, `jg`, `jge`):
  ```
  mov ax, a / cmp ax, [b]
  jae L_false              ; 73 05 — unsigned inverse of <
  ```
  The C type system (signed vs unsigned) drives
  the jcc family selection.
- `2170` (**long * long**): can't use single `imul`
  (which is 16×16 → 32). Calls helper:
  ```
  External: N_LXMUL@ (long multiply)
  
  ; setup:
  mov cx, a.hi / mov bx, a.lo
  mov dx, b.hi / mov ax, b.lo
  call N_LXMUL@           ; e8 [disp] with FIXUPP
  ; result: DX:AX (low 32 bits of product)
  ```
  
  N_LXMUL@ helper signature:
  - In: CX:BX = arg1, DX:AX = arg2
  - Out: DX:AX = product (low 32 bits)
  
  Adds to the helper-functions catalogue:
  N_LXMUL@, N_LDIV@, N_LXLSH@, N_LXRSH@, N_FTOL@,
  N_SCOPY@, N_SPUSH@, N_OVERFLOW@.

**Multi-byte arithmetic helpers** (BCC runtime):
| Helper | Operation | Input | Output |
|--------|-----------|-------|--------|
| `N_LXMUL@` | long multiply | CX:BX, DX:AX | DX:AX (low 32 bits) |
| `N_LDIV@` | long signed div | CX:BX (denom), DX:AX (num) | DX:AX (quot), CX:BX (rem) |
| `N_LXLSH@` | long left shift | DX:AX, CL (shift count) | DX:AX |
| `N_LXRSH@` | long right shift (signed) | DX:AX, CL | DX:AX |
| `N_FTOL@` | float→long | FPU TOP | DX:AX |
| `N_SCOPY@` | struct copy | DS:SI src, ES:DI dst, CX bytes | (memory) |
| `N_SPUSH@` | struct push | DS:SI src, CX bytes | (pushed on stack) |
| `N_OVERFLOW@` | stack overflow handler | (called when SP < __brklvl) | (terminates) |

For the Rust reimplementation:
- int*int: emit `imul m16`, discard DX.
- Unsigned cmp: use ja/jae/jb/jbe based on the C
  type signedness.
- long ops: emit external call to the appropriate
  N_LXXXX@ helper with the standard regs.

## `int f()` empty-paren = `(void)` in def; nested `#if` works; `#line` no OBJ effect

Fixtures `2165` (empty paren), `2166` (nested
`#if`), `2167` (`#line` directive) cover three
remaining syntactic patterns.

- `2165` (**`int trivial()` empty parens**): in a
  function **definition**, treated as `(void)`
  (no parameters). At call sites in K&R C, such
  a fn would accept any args (unchecked); BCC's
  parser handles both forms uniformly. OBJ
  identical to `int trivial(void)`.
- `2166` (**nested `#if`**): `#if LEVEL > 0 / #if
  LEVEL > 1 / #if LEVEL > 2 / #endif × 3` —
  each level evaluated independently at PP time.
  With LEVEL=2:
  - Outer: TAKEN (10 > 0)
  - Middle: TAKEN (LEVEL > 1)
  - Inner: NOT TAKEN
  
  Result: `x = 10 + 100 = 110`. Only the taken
  branches reach the compiler.
- `2167` (**`#line N "fname"`**): updates the
  preprocessor's idea of the current line and
  file. Used for `__LINE__`, `__FILE__`, warning/
  error message attribution. **No OBJ effect** —
  purely diagnostic.

**Preprocessor directive summary** (final):
| Directive | Effect | OBJ change |
|-----------|--------|------------|
| `#define X V` | Macro substitution | (indirect via expansion) |
| `#undef X` | Remove macro | (indirect) |
| `#ifdef X / #ifndef X / #endif` | Conditional inclusion | (indirect) |
| `#if expr / #elif / #else / #endif` | General conditional | (indirect) |
| `#include "f"` / `#include <f>` | File inclusion | (indirect via content) |
| `#pragma ...` | BCC-specific options | (varies) |
| `#error msg` | Compile error | (none — kills compile) |
| `#line N "fname"` | Override line/file | NONE |
| `defined(X)` | PP-expr operator | (indirect) |

**Function-declaration form-equivalence**:
| Form | Treatment in definition | Treatment in call |
|------|---------------------------|---------------------|
| `int f()` | No params (= void) | Any args (K&R: unchecked) |
| `int f(void)` | No params (= void) | Zero args required |
| `int f(int a, int b)` | Two params | Two args required |
| `int f(a, b) int a, b;` | K&R style, 2 params | Two args |
| `int f(int, ...)` | Varargs (at least 1) | At least 1 arg |

For the Rust reimplementation:
- Empty parens in fn def = void params.
- Nested `#if`: implement with a stack of "is
  this branch taken" flags.
- `#line` directive: update PP's source-location
  state; no codegen effect.

## Multi-fn PUBDEF = reverse-source, main last; implicit-int return ok; K&R fn syntax supported

Fixtures `2162` (3 fns + main PUBDEF order), `2163`
(implicit int return), `2164` (K&R-style decl)
cover three function-syntax behaviours.

- `2162` (**multi-fn PUBDEF order**): fns appear
  in PUBDEF as **reverse declaration order, with
  `main` last**:
  ```
  Source: a_fn, b_fn, c_fn, main
  PUBDEF: c_fn (offset 0x14), b_fn (0x0a),
          a_fn (0x00), main (0x1e)
  ```
  Helpers reversed; `main` placed at end. Each
  fn is its own PUBDEF record.
- `2163` (**implicit int return**): `helper(int x)
  { return x + 1; }` — no explicit return type
  defaults to `int` per K&R/C89. Same OBJ as
  `int helper(int x)`.
- `2164` (**K&R-style fn declaration**):
  ```c
  int add(a, b)
  int a, b;
  {
    return a + b;
  }
  ```
  Parameter declarations between the parameter
  list and body. Same OBJ as ANSI-style. BCC
  supports both styles.

**Function-syntax tolerance summary** (BCC accepts):
| Form | Status |
|------|--------|
| ANSI `int f(int a, int b)` | Standard |
| K&R `int f(a, b) int a, b;` | Supported (K&R-style) |
| Implicit-int `f(int x)` | Supported (K&R) |
| Implicit args `int f()` | Old-style, no arg checking |
| Function prototype `int f(int);` | Supported |
| Varargs `int f(int, ...)` | Supported |

**PUBDEF emission order** (combined):
| Symbol type | Order |
|-------------|-------|
| Variables (same segment) | Reverse declaration order |
| Functions (helpers) | Reverse declaration order |
| Function `main` | Always last in PUBDEF |
| Across segments | Within-segment order independent |

For the Rust reimplementation:
- Accept K&R fn syntax (or warn).
- Default return type = int when not specified.
- PUBDEF emission: reverse symbol list per
  segment, defer `main` to end.

## Globals laid out src-order in `_DATA`; PUBDEF emits reverse-order; init+uninit partitioned `_DATA`/`_BSS`

Fixtures `2159` (3 init globals), `2160` (mixed
init/uninit), `2161` (uninit global array) refine
global layout rules.

- `2159` (**multi-init globals, source order**):
  layout in `_DATA` follows source order (a at 0,
  b at 2, c at 4). PUBDEF entries emit in
  **reverse** (`_c, _b, _a`) — likely artifact of
  BCC's parser stack-pushing symbols. Layout is
  correct; PUBDEF order is just metadata.
- `2160` (**mixed init/uninit globals**):
  partitioning across segments:
  - `_DATA`: a at offset 0, c at offset 2 (skipping
    b which is uninit)
  - `_BSS`: b at offset 0
  
  PUBDEF: `_c` (DATA off 2), `_b` (BSS off 0), `_a`
  (DATA off 0). Each symbol references its own
  segment via the segment-index field.
  
  Init/uninit ordering in source doesn't change
  storage segment — only the offsets within each
  segment respect the source order of like-typed
  vars.
- `2161` (**uninit global array**): `int arr[5];`
  goes in `_BSS` with size 10 bytes (5 ints).
  Single PUBDEF for `_arr`. Access via `[disp16]`
  with FIXUPP per element offset.

**Global layout rules** (final):
1. Initialized globals → `_DATA`, in source-
   declaration order, packed without padding.
2. Uninitialized globals (tentative defs) →
   `_BSS`, in source-declaration order, packed.
3. Tentative defs and init defs **partitioned**
   into separate segments — relative offsets
   within each segment match source order of
   that type.
4. PUBDEF emits in **reverse declaration order**
   (artifact of BCC's parser).
5. Each PUBDEF entry includes the segment index
   so the linker knows where the symbol lives.

For the Rust reimplementation:
- Maintain two lists during parse: init-globals
  (going to `_DATA`) and tentative-defs (going to
  `_BSS`). Emit each in source order.
- PUBDEF emission: walk symbols in reverse order
  to match BCC's layout.

## Static global var = `_DATA` not exported; `extern var` = EXTDEF; uninit `int g;` = `_BSS` tentative

Fixtures `2156` (static global), `2157` (extern
var), `2158` (tentative def) characterise global
variable storage classes.

- `2156` (**`static int internal_g = 42;`**):
  goes in `_DATA` (segment 2, length 2) with the
  init value. **NOT in PUBDEF** — file-local
  symbol. Access via `a1 [disp16]` with FIXUPP
  to the local DATA segment.
- `2157` (**`extern int external_g;`**): no
  storage; **EXTDEF** entry for `_external_g`.
  Code references it via `a1 [disp16]` with
  FIXUPP. The OBJ won't link unless another OBJ
  provides `_external_g` as PUBDEF.
- `2158` (**`int g;` tentative def**): goes in
  **`_BSS`** (block-started-by-symbol — zero-
  filled at load time). Exported in PUBDEF at
  `_BSS` segment offset 0. Different from `_DATA`
  storage:
  ```
  SEGDEF for _BSS: length 2 (one int)
  PUBDEF: _g at _BSS offset 0
  ; main:
  c7 06 00 00 2a 00       ; mov word [_g], 42 (FIXUPP)
  a1 00 00                 ; mov ax, [_g] (FIXUPP)
  ```

**Global variable storage class summary**:
| Declaration | Storage | Symbol export | Init |
|-------------|---------|----------------|------|
| `int g = 42;` (file scope) | `_DATA` | PUBDEF | Explicit |
| `int g;` (tentative def) | `_BSS` | PUBDEF | Zero (load-time) |
| `static int g = 42;` | `_DATA` | (not exported) | Explicit |
| `static int g;` | `_BSS` | (not exported) | Zero |
| `extern int g;` | (none) | EXTDEF (referenced) | (def elsewhere) |
| `extern int g = 42;` | `_DATA` | PUBDEF | Explicit (overrides extern) |
| Local `static int g` | `_DATA` (fn-scoped) | (not exported) | Explicit or zero |

So **`_DATA` is for initialized globals**, **`_BSS`
is for zero-filled** (uninitialized or zero-init),
and **storage class** (static vs default) controls
PUBDEF emission.

For the Rust reimplementation:
- Tentative defs: emit to `_BSS` segment, not `_DATA`.
- `static` modifier: omit from PUBDEF.
- `extern` decl: emit EXTDEF; no storage.

## `extern fn` = EXTDEF + FIXUPP'd call; fwd-decl no impact; `static` fn = no PUBDEF, intra-seg call

Fixtures `2153` (extern fn), `2154` (fwd decl),
`2155` (static fn) cover function-symbol mechanics.

- `2153` (**`extern int printf(...)` call**): the
  external function generates an **EXTDEF** record
  in the OBJ symbol table. Call site uses `e8 00
  00` with FIXUPP to resolve at link time:
  ```
  ; symbol table:
  88 0a 00 07 _printf 00 71            ; EXTDEF
  
  ; main calls printf:
  mov ax, 0          ; b8 00 00 (FIXUPP for string)
  push ax
  call _printf       ; e8 00 00 (FIXUPP for fn addr)
  pop cx
  ```
  Varargs `(...)` in the prototype is just a
  type-check escape — doesn't affect call-site
  codegen for fixed-arity calls.
- `2154` (**forward decl**): `int helper(int x);`
  then later `int helper(int x) { ... }`. Forward
  decl is **purely type-system** — no codegen
  effect. Both `_helper` and `_main` are PUBDEF
  exports. Order in the OBJ matches source order.
- `2155` (**`static` fn**): file-local — **NO
  PUBDEF** for `_internal_helper`. Only `_main`
  is exported:
  ```
  ; PUBDEF section: only _main
  ; _internal_helper code is inline but invisible
  
  ; main calls helper via direct rel-near call:
  call _internal_helper       ; e8 e9 ff (intra-segment, NO FIXUPP)
  ```
  The static fn becomes invisible to other
  translation units. Call uses an internal
  relative offset — no link-time resolution
  needed.

**Function-symbol summary**:
| Modifier | OBJ effect | Call mechanism |
|----------|------------|-----------------|
| (default global) | PUBDEF (exported) | `e8 [rel]` intra-fn, or FIXUPP'd for ext |
| `static` | (not in PUBDEF) | `e8 [rel]` intra-segment |
| `extern` (no body) | EXTDEF (imported) | `e8 [rel]` with FIXUPP |
| Forward decl | (No symbol effect) | (Same as global) |

For the Rust reimplementation:
- `extern` w/ no body: emit EXTDEF; FIXUPP at
  call site.
- `static` fn: omit from PUBDEF; intra-segment
  call.
- Forward decl: type-system only.

## Double arr index = `BX = i; BX << 3; FLD m64 [BX]`; FLDZ/FLD1 for 0/1 consts; cmp w/ 0 = FLDZ+FCOMPP

Fixtures `2150` (double arr elem access), `2151`
(float != 0 cmp), `2152` (float * 2.0) cover
double-array indexing and FPU constant tricks.

- `2150` (**double arr indexed access**): index
  scaled by 8 (sizeof(double)) via 3 unrolled
  `shl bx, 1`:
  ```
  mov bx, i              ; load index
  shl bx, 1 × 3          ; multiply by 8 (= shift by 3)
  FLD m64 [bx+arr_base]   ; 9b dd 87 disp16 (with FIXUPP)
  ```
  ModR/M `87` = mod=10 reg=000 rm=111 (BX+disp16).
  
  Notable: even for `i = const 1` (parse-time
  known), BCC still emits the load+shift sequence
  — no constant-fold of array indexing.
  
  External symbol `__turboFloat` added: a
  flag-symbol that signals float code presence
  (linker uses it to detect float-using objects).
- `2151` (**float != 0 cmp uses FLDZ + FCOMPP**):
  loads 0.0 via the **FLDZ** instruction (no
  memory access), then FCOMPP to compare both
  values:
  ```
  9b d9 e8               ; FLD1 (= 1.0 const, for the assign of 1.0f)
  9b d9 5e fc            ; FSTP m32 [f]
  9b d9 46 fc            ; FLD m32 [f]
  9b d9 ee               ; FLDZ (load 0.0 to FPU)
  9b de d9               ; FCOMPP (de /3 — compare and pop both)
  9b dd 7e fa            ; FSTSW m16
  90 / 9b 8b / 9e / 74 05 ; status → AX → flags → je L_false
  ```
  Saves 4 bytes (no `0.0` constant in `_DATA`).
- `2152` (**float * 2.0** no special opt): BCC
  does NOT use `FADD ST(0), ST(0)` (which would
  double the value) or any other trick. Just
  loads 2.0 from `_DATA` and FMUL:
  ```
  9b d9 46 fc            ; FLD m32 [f]
  9b d8 0e 04 00         ; FMUL m32 [2.0]
  ```

**FPU constant instructions** (load specific
values without memory):
| Opcode | Mnemonic | Value |
|--------|----------|-------|
| `d9 e8` | FLD1 | 1.0 |
| `d9 e9` | FLDL2T | log2(10) |
| `d9 ea` | FLDL2E | log2(e) |
| `d9 eb` | FLDPI | π |
| `d9 ec` | FLDLG2 | log10(2) |
| `d9 ed` | FLDLN2 | ln(2) |
| `d9 ee` | FLDZ | 0.0 |

BCC uses **FLD1** and **FLDZ** for 1.0 and 0.0
constants. Other constants in source (like 2.0,
3.14) go through `_DATA` storage.

**Double-array stride encoding**:
- For `int i` index: `mov bx, i / shl bx, 1 × 3
  / FLD m64 [bx+disp16]` (8 bytes setup + 5 bytes
  FLD)
- Stride is hard-coded shift-by-log2(sizeof)
- Same pattern for float arrays (shift-by-2 = 4B
  stride), char arrays (no shift), etc.

For the Rust reimplementation:
- Float consts 0.0 and 1.0: emit FLDZ/FLD1.
- Other float consts: store in `_DATA` and FLD.
- Array indexing: emit shift-by-log2(stride) +
  indexed load.
- Track `__turboFloat` external for float-using
  objects.

## `-d` via FCHS (`d9 e0`); `d += K` = FADD m64; double const NOT exact-as-single stored as 8B

Fixtures `2147` (double negate), `2148` (compound
assign), `2149` (double ==) refine double-codegen
details.

- `2147` (**unary `-d` = FCHS**): emits `9b d9
  e0` (3 bytes total: WAIT + FCHS = `d9 /4`).
  Toggles the sign bit on FPU TOP. No memory
  access.
- `2148` (**`d += K` compound assign**): for
  commutative ops:
  ```
  FLD m32/m64 K              ; load constant
  FADD m64 [d]                ; top += d (commutative)
  FSTP m64 [d]                ; store back
  ```
  Uses `dc /0` (FADD m64) reading from `[d]`.
  Same pattern for `d *= K` with `dc /1` (FMUL
  m64).
- `2149` (**`a == b` double eq**): emits FCOMP
  m64 / FSTSW / SAHF / **`jne`** (not jbe/ja).
  Equality only needs ZF, so the unsigned-vs-
  signed distinction doesn't matter — `jne`
  works for both.
  
  Notable: const `3.14` here is stored as **full
  8-byte double** in `_DATA` (`1f 85 eb 51 b8 1e
  09 40`). 3.14 is NOT exactly representable as
  single, so the optimisation that would shrink
  it to 4 bytes (seen in 2136 with 1.5/2.5)
  doesn't apply.

**FPU misc opcodes** (`d9` family for m32-ish ops):
| Opcode | Mnemonic | Description |
|--------|----------|-------------|
| `d9 06 [m]` | FLD m32 | Load single |
| `d9 1e [m]` | FSTP m32 | Store+pop single |
| `d9 e0` | FCHS | Negate (toggle sign bit) |
| `d9 e1` | FABS | Absolute value |
| `d9 e4` | FTST | Test against 0 |
| `d9 e8` | FLD1 | Push 1.0 |
| `d9 ee` | FLDZ | Push 0.0 |
| `d9 fe` | FSIN | Sine (286+) |
| `d9 ff` | FCOS | Cosine (286+) |

**Double-constant single-optimisation rule**:
- Const exactly representable as single → store
  as 4 bytes in `_DATA`, load via FLD m32 (FPU
  widens to 80-bit naturally, FSTP m64 to var)
- Const NOT exactly representable → store as 8
  bytes in `_DATA`, load via FLD m64

For the Rust reimplementation:
- Unary `-` on float/double: emit FCHS.
- Compound assign with commutative op: load
  const, op-with-var, store.
- Double const storage: check exact-single
  representability; emit 4B or 8B accordingly.

## Float return on FPU TOP; `(float)int` via FILD m16; `float→double` auto-promotes via FLD m32/FSTP m64

Fixtures `2144` (double fn return), `2145` (int→
float), `2146` (float→double promotion at call)
characterise float conversion mechanisms.

- `2144` (**double return on FPU TOP**): callee
  leaves result on the FPU stack, NOT in AX or
  DX:AX. Caller picks up the FPU TOP after the
  call:
  ```
  ; callee:
  9b dd 46 04            ; FLD m64 [bp+4] (load arg)
  9b d8 36 [disp]        ; FDIV m32 const
  ; result on FPU TOP
  pop bp / ret           ; return — result still on FPU stack
  
  ; caller, after the call:
  ; FPU TOP holds the result; can FSTP or FCOMP
  ```
  Different from int returns (AX) or long returns
  (DX:AX).
- `2145` (**`(float)int` via FILD**): integer→float
  conversion uses **`FILD m16`** (load int into
  FPU), then `FSTP m32` to store as float:
  ```
  mov [tmp], i             ; spill int to memory
  9b df 46 fa              ; FILD m16 (load 16-bit int to FPU)
  9b d9 5e fa              ; FSTP m32 (store as float)
  ```
- `2146` (**`float → double` auto via FPU**):
  promotion happens **implicitly** through the
  FPU's internal precision. Load as m32, store
  as m64:
  ```
  9b d9 46 fc              ; FLD m32 (load float)
  ; FPU internally has 80-bit value
  9b dd 5e f4              ; FSTP m64 (store as double)
  ```
  No explicit conversion call needed.

**Float conversion summary**:
| From | To | Mechanism | Cost |
|------|-----|-----------|------|
| `int` | `float`/`double` | `FILD m16` + FSTP m32/m64 | ~10 bytes |
| `float` | `double` | `FLD m32 / FSTP m64` (FPU widens) | 8 bytes |
| `double` | `float` | `FLD m64 / FSTP m32` (FPU narrows) | 8 bytes |
| `float`/`double` | `int` | `FLD / call N_FTOL@` | ~7 bytes |
| `float`/`double` | `long` | `FLD / call N_FTOL@` | ~7 bytes |

**Return-value conventions** (updated):
| Type | Return register |
|------|------------------|
| `char` | AL (high byte undef) |
| `int`, `short`, near ptr | AX |
| `long`, far ptr | DX:AX |
| `float`, `double` | FPU TOP (ST(0)) |
| `struct ≤ 4 bytes` | AX or DX:AX |
| `struct > 4 bytes` | via N_SCOPY@ (callee writes to a slot) |

For the Rust reimplementation:
- Float return: leave on FPU stack; caller pops
  as needed.
- int→float: spill int + FILD m16 + FSTP m32/m64.
- float↔double: FLD/FSTP with different precisions.

## FMUL = `d8 /1`, FDIV = `d8 /6` (m32); passing `double` arg = `add sp,-8` + FSTP m64 [sp]

Fixtures `2141` (float mul), `2142` (float div),
`2143` (passing double as arg) complete the
float-operator and float-call survey.

- `2141` (**float mul**): `9b d8 /1 [m]` = FMUL
  m32. ModR/M `4e` = /1 reg-field.
- `2142` (**float div**): `9b d8 /6 [m]` = FDIV
  m32. ModR/M `76` = /6 reg-field.
- `2143` (**passing `double` to fn**): caller uses
  **`add sp, -8`** (allocate, NOT push) + FSTP
  m64 to fill the slot, then call:
  ```
  ; In main:
  9b d9 06 [const_addr]      ; FLD m32 (7.5 stored as single)
  83 c4 f8                    ; add sp, -8 (subtract 8)
  9b dd 5e f8                 ; FSTP m64 [sp] (store as double in arg slot)
  90                           ; NOP
  9b e8 de ff                 ; WAIT + call dbl_to_int
  83 c4 08                    ; add sp, 8 (cleanup)
  ```
  Cleanup is `add sp, 8` (3 bytes) instead of 4
  pops, since 8-byte cleanup > 4-byte threshold.
  Callee accesses double arg at `[bp+4]`:
  ```
  9b dd 46 04                 ; FLD m64 [bp+4]
  ```

**FPU `d8 /reg` family complete** (m32 arith):
| `/reg` | Mnemonic | Description |
|--------|----------|-------------|
| `/0` | FADD m32 | Add |
| `/1` | FMUL m32 | Multiply |
| `/2` | FCOM m32 | Compare (no pop) |
| `/3` | FCOMP m32 | Compare + pop |
| `/4` | FSUB m32 | Subtract |
| `/5` | FSUBR m32 | Reverse subtract |
| `/6` | FDIV m32 | Divide |
| `/7` | FDIVR m32 | Reverse divide |

Similar `dc /reg` family exists for m64 (double-
precision) operations.

**Float arg passing summary**:
| Arg type | Caller emit | Bytes |
|----------|-------------|-------|
| `float` | sub sp, 4 + FSTP m32 [sp] | 6+ |
| `double` | sub sp, 8 (= add sp, -8) + FSTP m64 [sp] | 6+ |
| Promote `float` → `double` at call | (same as `double`) | (FPU's natural extension) |

For the Rust reimplementation:
- FPU `d8 /reg` family for m32 arith; `dc /reg`
  for m64.
- Float arg passing: `add sp, -N` + FSTP m32/m64
  pattern.
- Always emit WAIT (`9b`) before each FPU op.

## Float add = `9b d8 /0` (FADD m32); double cmp = FCOMP/FSTSW/SAHF + unsigned jcc; double arrs = 8B

Fixtures `2138` (float add), `2139` (double cmp),
`2140` (double array) characterise FPU operations
in detail.

- `2138` (**float add**): uses **`9b d8 /0`**
  (FADD m32, single-precision add):
  ```
  9b d9 46 fc          ; FLD m32 [a]
  9b d8 46 f8          ; FADD m32 [b]
  9b d9 5e f4          ; FSTP m32 [s]
  ```
  FPU opcodes by precision/family:
  - `d8` = m32 arith operations
  - `d9` = m32 load/store + misc FPU
  - `dc` = m64 arith operations
  - `dd` = m64 load/store + misc FPU
- `2139` (**double comparison**): uses the
  FCOMP/FSTSW/SAHF pattern:
  ```
  9b d9 06 [disp]        ; FLD m32 (3.5 const, single in _DATA)
  9b dd 5e f8            ; FSTP m64 [d] (store as double)
  9b dd 46 f8            ; FLD m64 [d]
  9b d8 1e [disp]        ; FCOMP m32 (compare against 2.0, pop)
  9b dd 7e f6            ; FSTSW m16 (store status word)
  90                      ; NOP
  9b 8b 46 f6            ; mov ax, status
  9e                      ; SAHF (set flags from AH)
  7e 05                  ; jle/jbe (UNSIGNED jcc — FPU maps to above/below)
  ```
  External `FIWRQQ` (FPU init/word) added.
  
  Float cmp emits **unsigned jcc** (`ja`, `jb`,
  `jae`, `jbe`) because FPU comparison results
  map to "above/below/equal" — not "signed".
- `2140` (**double array**): elements stored as
  **full 8 bytes each** in `_DATA`:
  ```
  ; 3 doubles: 00 00 00 00 00 00 f0 3f (1.0)
  ;            00 00 00 00 00 00 00 40 (2.0)
  ;            00 00 00 00 00 00 08 40 (3.0)
  ; Total 24 bytes.
  ```
  Access: `9b dd 06 [offset]` (FLD m64).
  
  Contrast with scalar double init (2136): scalar
  consts optimised to single (4B) when exactly
  representable. Array elements must be full 8B
  for indexed access.

**FPU instruction summary** (key opcodes):
| Opcode | Mnemonic | Description |
|--------|----------|-------------|
| `d8 /0` | FADD m32 | Add single |
| `d8 /3` | FCOMP m32 | Cmp+pop single |
| `d9 06 [m]` | FLD m32 | Load single |
| `d9 1e [m]` | FSTP m32 | Store+pop single |
| `dc /0` | FADD m64 | Add double |
| `dc /3` | FCOMP m64 | Cmp+pop double |
| `dd 06 [m]` | FLD m64 | Load double |
| `dd 1e [m]` | FSTP m64 | Store+pop double |
| `dd 3e [m]` | FSTSW m16 | Status word to memory |
| `df e0` | FSTSW ax | Status word to AX (286+) |
| `9b ...` | WAIT prefix | Sync 8087 |

**Float vs double storage**:
| Context | Float | Double |
|---------|-------|--------|
| Scalar var | 4B stack | 8B stack |
| Scalar const | 4B in _DATA | 4B if exact-single, else 8B |
| Array elem | 4B stride | 8B stride |
| Struct field | 4B | 8B |

For the Rust reimplementation:
- Emit FPU instructions per precision (m32 vs m64).
- Use unsigned jcc after FCOMP+FSTSW+SAHF.
- Detect exact-single representable double consts
  for scalar (NOT array) storage optimisation.

## `-2` byte-identical to `-1` for trivial; `double` const stored as single in `_DATA`; `-f-` no-op trivial

Fixtures `2135` (-2 286 target), `2136` (double
arith), `2137` (-f- no float flag) complete the
target/float survey.

- `2135` (**`-2` 286 target**): byte-identical to
  `-1` (80186) for trivial cases. Both use ENTER/
  LEAVE/shl-imm. Differentiating tests would need
  286-specific instructions (CMPSW, BOUND, etc.)
  in the source.
- `2136` (**double arithmetic**): doubles use
  **8 bytes on stack**, but **constants exactly
  representable in single-precision are stored as
  4-byte singles** in `_DATA`:
  ```
  ; data: 1.5 = 00 00 c0 3f (single), 2.5 = 00 00 20 40
  ; total 8 bytes for two double "constants"
  ```
  Operations use double-precision FPU instructions:
  - `9b d9 06 [disp]` = FLD m32 (load single from data)
  - `9b dd 5e f8` = FSTP m64 (store as double on stack)
  - `9b dc 46 f0` = FADD m64 ([bp-16] = e)
  - `9b dd 46 e8` = FLD m64 (load double for return)
  
  The FPU internally uses 80-bit precision, so
  loading a single and storing as double is
  lossless if the source value is exactly
  representable as single.
- `2137` (**`-f-` disable float emulation**):
  for int-only code, byte-identical to default.
  The flag only affects link-time library
  selection.

**Float-related flag summary**:
| Flag | Codegen effect | Linkage effect |
|------|----------------|-----------------|
| (default) | Emit FPU ops with `9b` WAIT | Link emulation library |
| `-f` | Same as default | (same) |
| `-f87` | Same FPU ops | Skip emulation; require 8087 hardware |
| `-f-` | Same for int-only code | Don't link float library at all |
| `-f287` | Same | Require 80287 (286+ FPU) |

For the Rust reimplementation:
- Track `float`/`double` types; emit FPU ops with
  WAIT prefix.
- Optimise small `double` constants to single-
  precision storage when representable exactly.
- Float-flag selection mainly drives library
  linkage; codegen is largely flag-agnostic.

## Float = FPU `9b d9/...` + N_FTOL@; `-1` enables 80186 ENTER/LEAVE/shl-imm; IEEE 754 single in `_DATA`

Fixtures `2132` (float emulation), `2133` (-1
target), `2134` (-1 + shift by 4) cover float
support and 80186-target codegen.

- `2132` (**float = FPU + 8087 emulation library**):
  ```
  9b d9 06 [disp]          ; WAIT + FLD m32 (load float)
  9b d9 5e fc              ; WAIT + FSTP m32 (store)
  9b d9 46 fc              ; WAIT + FLD m32 (load back)
  e8 [disp]                 ; call N_FTOL@ (float→long conversion)
  ```
  - `9b` prefix = WAIT (FPU sync for 8086 boards
    with separate 8087 chip)
  - FPU opcodes `d8`-`df` (FLD, FSTP, FADD, etc.)
  - Helper symbols: `FIDRQQ` (emulation library
    entry), `N_FTOL@` (float-to-long)
  
  Constants like `3.14f` stored as **IEEE 754
  single-precision** in `_DATA`: `c3 f5 48 40` =
  0x4048F5C3 = 3.14.
- `2133` (**`-1` enables 80186 target**): emits
  shorter/newer instructions:
  - **`ENTER imm16, imm8`** (`c8 04 00 00`) =
    push bp / mov bp, sp / sub sp, 4. 4 bytes
    vs 5 bytes for the discrete sequence.
  - **`LEAVE`** (`c9`) = mov sp, bp / pop bp. 1
    byte vs 3 bytes.
  - **`shl reg, imm8`** (`c1 /4 reg imm8`) =
    direct shift-by-N. 3 bytes regardless of N.
    Removes the CL-form `mov cl, N / shl reg,
    cl` (4 bytes) and the unrolled shl-by-1 ×
    N for small N.
- `2134` (**-1 + shift by 4**): just `c1 e0 04`
  (3 bytes). Confirms 80186 shift is always 3
  bytes — no threshold-based switching.

**8086 vs 80186 shift comparison**:
| Shift N | 8086 (default) | 80186 (-1) |
|---------|----------------|-------------|
| 1 | `d1 e0` (2B) | `c1 e0 01` (3B) |
| 2 | 2× `d1 e0` (4B) | `c1 e0 02` (3B) |
| 3 | 3× `d1 e0` (6B) | `c1 e0 03` (3B) |
| 4+ | `mov cl, N / d3 e0` (4B) | `c1 e0 N` (3B) |

So **80186 shift is uniform 3 bytes**, beating
8086 for N ≥ 2.

**80186 prologue/epilogue replacement**:
| 8086 | 80186 (-1) |
|------|-------------|
| `55 8b ec 83 ec N` (6B) | `c8 N 00` (4B for N ≤ 255) |
| `8b e5 5d c3` (4B) | `c9 c3` (2B) |

Saves ~4 bytes per function.

For the Rust reimplementation:
- Track `-1` flag; emit ENTER/LEAVE/shl-imm
  variants instead of discrete instructions.
- Float: emit FPU instructions with `9b` WAIT
  prefix; call N_FTOL@ for float-to-int.
- IEEE 754 single-precision encoding for `float`
  literals.

## `-N` stack-overflow check via `N_OVERFLOW@`; `-K` char unsigned (zero-ext); `-D` CLI define = #define

Fixtures `2129` (-N stack check), `2130` (-K
unsigned char), `2131` (-D CLI define) cover three
more BCC flags.

- `2129` (**`-N` stack overflow checking**): each
  function with stack allocation gets an
  overflow-check prologue:
  ```
  push bp / mov bp, sp
  sub sp, 0x28              ; allocate locals (40 bytes here)
  cmp [__brklvl], sp        ; compare break level vs sp
  jb skip                    ; jb = jump if below (sp > brklvl = safe)
  call N_OVERFLOW@           ; otherwise call overflow handler
  ```
  External refs introduced: `N_OVERFLOW@`,
  `___brklvl`. Adds ~8 bytes per stack-frame
  function. Useful for catching stack overruns
  at runtime.
- `2130` (**`-K` default char is unsigned**):
  changes the default signedness of `char` from
  signed to unsigned:
  ```
  ; With -K (char = unsigned):
  mov al, [c] / mov ah, 0      ; zero-extend
  
  ; Default (char = signed):
  mov al, [c] / cbw             ; sign-extend
  ```
  Affects all `(int)char_val` conversions. C
  standard leaves char signedness implementation-
  defined; BCC defaults to signed, `-K` makes it
  unsigned.
- `2131` (**`-D<name>=<value>` CLI define**):
  identical to `#define <name> <value>` at PP.
  `-DFOO=42` makes the `#ifdef FOO` branch active
  and `FOO` substitute as 42. Useful for build-
  config.

**Flag-effect master summary** (codegen-relevant):
| Flag | Effect | Output bytes (vs default) |
|------|--------|----------------------------|
| `-O` | Remove trailing `eb 00` | -2 per expr |
| `-d` | Merge dup string literals | -strlen per dup |
| `-G` | (no observable trivial diff) | 0 |
| `-r-` | Disable register alloc | +varies (worse) |
| `-N` | Stack overflow check | +8 per stack-fn |
| `-K` | char defaults unsigned | changes cbw → mov ah, 0 |
| `-Dx=y` | Define macro at CLI | (PP-time only) |
| `-Ux` | Undefine macro at CLI | (PP-time only) |
| `-Ipath` | Add include path | (PP-time only) |

For the Rust reimplementation:
- `-N`: emit the stack-check prologue when set.
- `-K`: track char signedness via flag; emit
  cbw or mov ah, 0 accordingly.
- `-D`/`-U`: feed into PP's macro table before
  source processing.

## `-O` consistent across expr sites; `-r-` disables reg-alloc (vars in mem); recursion -O removes ~4B

Fixtures `2126` (-O multi expr), `2127` (-r-
disable reg-alloc), `2128` (-O recursive
factorial) confirm/refine flag effects.

- `2126` (**-O across multi-expr**): no `eb 00`
  between expressions or before epilogue. Both x
  and y still enregister (SI/DI) — `-O` doesn't
  change register allocation, just removes the
  trailing no-op.
- `2127` (**`-r-` disables register allocation**):
  3 locals → 3 stack slots (`83 ec 06` for 6
  bytes). All accesses via `[bp+disp]`. None
  enregistered into SI/DI/BX:
  ```
  c7 46 fe 05 00          ; x = 5 (stack)
  c7 46 fc 0a 00          ; y = 10 (stack)
  c7 46 fa 14 00          ; z = 20 (stack)
  mov ax, [x] / add ax, [y] / add ax, [z]
  ```
  Larger code, more memory traffic. Useful only
  for debug/analysis.
- `2128` (**-O recursive factorial**): `-O`
  removes `eb 00` at every expression site. In
  recursive functions with multiple expressions,
  saves 2 bytes per site. Net savings = 2 × N
  sites.

**Optimization-flag effects** (full):
| Flag | Effect | Per-fn bytes saved |
|------|--------|---------------------|
| `-O` | Remove `eb 00` no-ops at expression ends | ~2 × #expr |
| `-d` | Merge duplicate string literals | varies |
| `-G` | (no observable trivial effect) | 0 |
| `-r` (default on) | Enable register allocation | (vars in SI/DI/BX) |
| `-r-` | DISABLE register allocation | -varies (worse code) |

**Register-allocation control summary**:
- Default (`-r` on): up to ~3 enregistered ints
  per function (pool {SI, BX, DI, CX, DX} per
  rules).
- `-r-`: all locals on stack.

For the Rust reimplementation:
- `-O`: trivially implementable as "don't emit
  the trailing `eb 00`."
- `-r-`: skip the register-allocation phase;
  emit every local as a stack slot.

## `-O` strips trailing `eb 00` no-op (-2B per expr); `-d` merges string dupes; `-G` no observable effect

Fixtures `2123` (-G flag), `2124` (-d merge
strings), `2125` (-O jump opt) probe BCC command-
line flags' codegen effects.

- `2123` (**`-G` flag**): byte-identical to default
  for the trivial case. May affect more complex
  cases (it's documented as "select for speed").
  No observable effect here.
- `2124` (**`-d` merge duplicate strings**):
  identical string literals `"hello"` and
  `"hello"` (declared in separate global decls)
  are **merged** to a single copy in `_DATA`.
  Both pointers reference the same offset. With
  `-d`, `a == b` is true; without it, they're
  separate copies.
- `2125` (**`-O` jump optimization**): strips the
  trailing **`eb 00` (jmp +0)** no-op that BCC
  normally emits at the end of expressions:
  ```
  ; Default:
  ... 33 c0 / eb 00 / 8b e5 / 5d c3  (15 bytes)
  
  ; With -O:
  ... 33 c0 / 8b e5 / 5d c3  (13 bytes)
  ```
  Saves **2 bytes per expression site**. Also
  shortens preceding jcc distances. The
  ubiquitous `eb 00` we've seen throughout the
  corpus is BCC's structural no-op — `-O`
  recognizes and removes it.

**BCC command-line flag summary** (codegen-
relevant):
| Flag | Effect |
|------|--------|
| `-c` | Compile only (no link) |
| `-ms` / `-mc` / `-mm` / `-ml` / `-mh` | Memory model |
| `-O` | Optimize jumps — removes `eb 00` no-ops |
| `-G` | Optimize for speed (no observable diff trivial) |
| `-d` | Merge duplicate strings in `_DATA` |
| `-w<class>` | Warning control |
| `-D<name>=<val>` | Define preprocessor symbol |
| `-U<name>` | Undefine preprocessor symbol |
| `-I<dir>` | Include path |
| `-v` | Source debug info |

For the Rust reimplementation:
- Match `-O` by removing the trailing-`eb 00`
  emission (and shortening preceding jcc by 2).
- Match `-d` by deduplicating string literals
  in `_DATA` at link/emission time.

## `asm { ... }` block syntax; `#pragma warn` PP-only; pseudo-registers `_AX`/etc. in C exprs

Fixtures `2120` (asm block), `2121` (pragma warn),
`2122` (pseudo-reg + asm) cover BCC extensions.

- `2120` (**`asm { ... }` block**): multi-line
  inline assembly in braces. Each line emits one
  instruction. Equivalent to multiple `asm
  <instr>;` statements:
  ```c
  asm {
    mov ax, x
    add ax, 5
    mov x, ax
  }
  ```
  Output: `8b 46 fe / 05 05 00 / 89 46 fe` (9
  bytes). Same as the separate-line form.
- `2121` (**`#pragma warn -ccc`**): disables a
  specific warning class at PP level. **No OBJ
  effect** — the directive only influences the
  compiler's warning emission, not the code.
- `2122` (**pseudo-registers `_AX`, `_BX`, ...**):
  Borland-specific: `_AX` in a C expression reads
  the current AX register value. Combined with
  inline asm to do byte-swap:
  ```c
  asm mov ax, x          // load x into AX
  asm xchg ah, al        // swap bytes (86 c4)
  return _AX;            // return current AX
  ```
  No `mov ax, ax` shuffling — `_AX` directly
  exposes the register. Result: `0x1234` → `0x3412`
  (byte-swapped).

**Pseudo-register summary** (BCC extension):
| Pseudo | Register |
|--------|----------|
| `_AX`, `_BX`, `_CX`, `_DX` | 16-bit GP |
| `_AL`, `_BL`, `_CL`, `_DL` | low 8-bit halves |
| `_AH`, `_BH`, `_CH`, `_DH` | high 8-bit halves |
| `_SI`, `_DI`, `_BP`, `_SP` | index/stack |
| `_CS`, `_DS`, `_SS`, `_ES` | segment |
| `_FLAGS` | flags word (some variants) |

These are useful when interfacing inline asm with
C code — the C statement after the asm can pick
up the asm's result without an explicit `mov` to
a C variable.

For the Rust reimplementation:
- Parse `asm { ... }` block syntax.
- Pragmas: emit as no-op directives, with side
  effects on warning state.
- Pseudo-registers: parse as primary expressions
  that map directly to register operands.

## `#undef`+`#define` redefines; `defined()` operator; `asm` keyword = literal inline assembly

Fixtures `2117` (undef), `2118` (defined()), `2119`
(asm) cover three preprocessor/extension idioms.

- `2117` (**`#undef` then `#define` redefines**):
  ```c
  #define X 10
  int a = X;        // a = 10
  #undef X
  #define X 99
  int b = X;        // b = 99
  ```
  Macros are dictionary-style: definition-order
  matters. Use-site reflects the definition at
  that point.
- `2118` (**`defined()` operator in `#if`**):
  `#if defined(NAME)` ≡ `#ifdef NAME`, and `#if
  !defined(NAME)` ≡ `#ifndef NAME`. Both
  recognised in BCC's PP.
- `2119` (**`asm` keyword**): Borland-specific
  inline assembly. Each `asm <instr>` emits one
  literal assembly instruction:
  ```c
  asm mov ax, x;             // emits 8b 46 fe (mov ax, [bp-2])
  asm add ax, 1;             // emits 05 01 00 (add ax, 1, AX-form imm16)
  ```
  The inline assembler **does NOT optimise**:
  `asm add ax, 1` emits the literal `05 01 00`
  (3 bytes), NOT `inc ax` (1 byte). This contrasts
  with BCC's normal `+1` optimisation for C code.
  
  Multiple `asm` statements can be chained. Local
  C variables (`x`) can be referenced — BCC
  generates the right `[bp+disp]` for them.

**Preprocessor/extension summary (updated)**:
| Construct | Effect |
|-----------|--------|
| `#define X V` | Macro substitution |
| `#undef X` | Remove macro |
| `#if defined(X)`, `#ifdef X` | Conditional compilation |
| `#if !defined(X)`, `#ifndef X` | Inverse |
| `asm <instruction>` | Inline ASM (Borland extension, literal — no opts) |

For the Rust reimplementation:
- Preprocessor: dictionary semantics for define/
  undef; track definition order.
- `defined()` operator: implement in PP-expression
  evaluator.
- `asm` keyword: parse as Borland-specific
  statement; emit literal opcodes from the
  assembler.

## `//` comments supported (extension); `#define` expanded at PP; `#ifdef` removes untaken branch

Fixtures `2114` (C++ comments), `2115` (#define
macros), `2116` (#ifdef) cover preprocessor
behaviour.

- `2114` (**C++-style `//` comments**): BCC 2.0
  **supports `//` comments as an extension** (not
  part of C89). Same OBJ output as if the
  comments weren't there — stripped at PP.
- `2115` (**`#define` macros**): both object-like
  (`#define MAX 100`) and function-like (`#define
  DOUBLE(x) ((x)*2)`) macros expand at PP.
  `DOUBLE(20)` substitutes to `((20)*2) = 40`,
  which constant-folds. Compiler sees only the
  post-expansion source. Symbol `MAX` becomes
  literal 100 wherever used.
- `2116` (**`#ifdef`/`#else`/`#endif`**): resolves
  at preprocessing. Only the **taken branch** is
  in the compiled OBJ. The untaken branch is
  invisible — no conditional code at all.

**Preprocessor summary**:
| Directive | Resolution | Output effect |
|-----------|------------|----------------|
| `//` comment | Lex strip | None |
| `/* */` comment | Lex strip | None |
| `#define X 100` | Lex substitution | `X` → `100` |
| `#define F(x) ((x)*2)` | Lex substitution + paren | Function-like macro expansion |
| `#ifdef X / #else / #endif` | PP-time | Only one branch compiled |
| `#include "file"` | PP-time | File inlined |
| `#include <file>` | PP-time | System file inlined |
| `#pragma`, `#error`, etc. | (BCC-specific) | Various |

So preprocessing is **fully lexical** and runs
before BCC's tokenizer sees the source.

For the Rust reimplementation:
- Implement `//` comments alongside `/* */`.
- Macro expansion in lex/PP phase.
- `#ifdef` etc. control inclusion before the
  parser runs.

## Char literal = parse-time int; `\xNN`/`\NNN` byte escapes; adjacent strings concat at parse

Fixtures `2111` (char literals), `2112` (hex/
octal escapes in strings), `2113` (adjacent
string concat) cover three more lexer behaviours.

- `2111` (**char literal = parse-time int**):
  `'A'`, `'\n'`, `'\0'` evaluate to 65, 10, 0
  respectively at parse time. Each fits in
  `imm16`, stored via `c7 46 disp imm16`. Per
  C standard, char literals have type `int`.
- `2112` (**hex/octal byte escapes**): `"\xAB
  \101\x7F"` decodes to `ab 41 7f`:
  - `\xNN` = hex byte (1-2 hex digits)
  - `\NNN` = octal byte (1-3 octal digits, `\101
    = 65 = 'A'`)
  
  Both are byte-valued, not int — they fit in a
  single char.
- `2113` (**adjacent strings concatenate at
  parse**): `"Hello, " "World!"` emitted as the
  single string `Hello, World!` (13 chars + null
  = 14 bytes in `_DATA`). Concatenation is a
  lexical/preprocessing step per C standard. NO
  runtime concat — single literal.

**Lexer summary (lex-time data)**:
| Token | Result | Type |
|-------|--------|------|
| `0xABCD` | hex int | `int` (or `unsigned int` if > INT_MAX) |
| `0177` | octal int | `int` (= 127) |
| `100` | decimal int | `int` |
| `'A'` | int (65) | `int` |
| `'\n'` | int (10) | `int` |
| `'\xAB'` | int (171) | `int` |
| `"abc"` | char[4] | `char[]` (with implicit `\0`) |
| `"a" "b"` | concatenated string | single `char[]` |
| `'\101'` (in string) | octal byte (65) | byte in string |

For the Rust reimplementation:
- Lex char literal → int value (parse the escape).
- Lex string literal: parse all escapes, concat
  adjacent strings, append implicit `\0`.

## `goto label` = `jmp`; escape sequences decode at parse; octal literals (leading 0)

Fixtures `2108` (goto), `2109` (string escapes),
`2110` (octal/hex literals) cover three lexical
and control-flow patterns.

- `2108` (**`goto label`**): emits **unconditional
  `jmp`** to the label address. Same as a while-
  loop's back-edge:
  ```
  start:
    inc si              ; x++
    cmp si, 5
    jge skip
    jmp start           ; eb f8 (goto)
  skip:
  ```
  No structural difference between `goto` and an
  unconditional loop edge.
- `2109` (**string escape sequences**): `"\n\t\r
  \\\\"` decodes to `0a 09 0d 5c 5c 00` (with
  implicit null at end). All standard C escapes:
  | Escape | Value |
  |--------|-------|
  | `\n` | 0x0a |
  | `\t` | 0x09 |
  | `\r` | 0x0d |
  | `\b` | 0x08 |
  | `\f` | 0x0c |
  | `\v` | 0x0b |
  | `\a` | 0x07 |
  | `\0` | 0x00 |
  | `\\` | 0x5c |
  | `\"` | 0x22 |
  | `\'` | 0x27 |
  | `\?` | 0x3f |
  | `\xNN` | hex byte |
  | `\NNN` | octal byte |
- `2110` (**integer literals**): three syntaxes:
  - `0x...` (hex): `0xABCD = 0xABCD = 43981`
  - `0...` (leading 0 = **octal**): `0177 = 127`
  - `N...` (no leading 0 = decimal): `1000 = 1000`
  
  Each emits via `c7 46 disp imm16` for stack
  init.

For the Rust reimplementation:
- `goto`: emit `jmp` to the labelled address.
- Lex char escapes per the table above.
- Lex integer literals: detect `0x` prefix (hex),
  `0` prefix with no `x` (octal), otherwise
  decimal.

## Cross-byte bitfield = word op; signed bitfield read = `shl/sar` sign-extend; 1-bit `=1` skips clear

Fixtures `2105` (1-bit flag bitfields), `2106`
(cross-byte bitfield), `2107` (signed bitfield)
deepen the bitfield characterisation.

- `2105` (**1-bit flag, multi-byte spanning field**):
  ```c
  struct Flags { unsigned f1:1; unsigned f2:1; unsigned val:14; };
  ```
  - `fl.f1 = 1`: just `or byte [bp+disp], 1` (no
    clear-first — assumes bit was 0; works only
    if storage was zero-initialised, **buggy
    for auto locals**).
  - `fl.f2 = 0`: `and byte [bp+disp], 0xfd`
    (clear bit).
  - `fl.val = 1000`: val spans bits 2..15, so
    needs WORD ops: `and word [m], 0x0003` (clear
    val bits) + `or word [m], 0x0fa0` (set val
    bits = 1000 << 2).
- `2106` (**cross-byte spanning bitfield**):
  ```c
  struct Cross { unsigned lo:6; unsigned mid:6; unsigned hi:4; };
  ```
  Storage: lo at byte 0 bits 0..5, mid spans
  byte 0 bits 6..7 + byte 1 bits 0..3, hi at
  byte 1 bits 4..7.
  
  For mid (crosses byte boundary): word op
  required:
  ```
  and word [bp+disp], 0xf03f      ; clear mid bits
  or  word [bp+disp], 0x0a00       ; set mid = 40 << 6
  ```
- `2107` (**signed bitfield read = shl/sar**):
  for `int x : 4`, read uses sign-extension via
  shift-left-then-arithmetic-right:
  ```
  mov al, [bp+disp]
  mov cl, 12 / shl ax, cl          ; align field in bits 12..15
  mov cl, 12 / sar ax, cl          ; arithmetic right = sign-extend
  ```
  10 bytes per signed-bitfield read. Beautiful
  8086 trick — putting the field in the high
  bits and arithmetic-shifting back fills the
  high bits with the sign.

**Bitfield encoding summary**:
| Operation | Single-byte field | Cross-byte field |
|-----------|-------------------|-------------------|
| Write `field = K` (unsigned, K fits) | `and byte [m], mask / or byte [m], K<<shift` (8 bytes) | `and word [m], mask / or word [m], K<<shift` (10 bytes) |
| Write `field = 1` (1-bit) | `or byte [m], (1<<shift)` (4 bytes — clear skipped) | (same word) |
| Read unsigned | `mov al, [m] / shr / and width-mask` | `mov ax, [m] / shr / and width-mask` |
| Read signed | `mov al, [m] / shl (16-w-pos) / sar (16-w)` | same with word load |

So **cross-byte bitfields force word ops**; signed
bitfields use the sign-extending shift trick.

For the Rust reimplementation:
- Detect when bitfield spans byte boundary →
  emit word-sized and/or.
- For `unsigned 1-bit = 1`: skip the clear (BCC's
  optimization — but be aware of correctness
  caveats for uninitialised auto vars).
- Signed bitfield read: shl to align in high
  bits, sar back to extract.

## Nested struct flat-laid; union shares storage (little-endian observable); bitfields packed LSB-first

Fixtures `2102` (nested struct init), `2103`
(union), `2104` (bitfields) cover three composite-
type patterns.

- `2102` (**nested struct flat-laid**):
  ```c
  struct Outer { int id; struct Inner inner; int tail; };
  static struct Outer o = {1, {10, 20}, 100};
  ```
  Data emits 8 bytes flat: `01 00 0a 00 14 00 64
  00`. Brace nesting in init parses but
  doesn't change layout — same as `{1, 10, 20,
  100}` with flat layout. Inner fields accessed
  as direct offsets from Outer's base.
- `2103` (**union shares storage, little-endian**):
  `u.as_int = 0x4142` writes a word; reading via
  `u.as_bytes[0]` returns 0x42 (low byte), `u.
  as_bytes[1]` returns 0x41 (high byte). **Little-
  endian observable**. Union size = max(member
  sizes).
- `2104` (**bitfields packed LSB-first**):
  ```c
  struct Bits { unsigned a:3; unsigned b:5; unsigned c:8; };
  ```
  Storage: 2 bytes total. Layout:
  - byte 0 bits 0..2 = a (LSB-first)
  - byte 0 bits 3..7 = b
  - byte 1 = c
  
  Write pattern (e.g., `bf.a = 5`):
  ```
  and byte [bp+disp], 0xf8        ; clear field bits
  or  byte [bp+disp], 5            ; set new value
  ```
  Read pattern (e.g., `bf.b`):
  ```
  mov al, [bp+disp]
  shr ax, 3 (× 3, unrolled)        ; shift field to LSB
  and ax, 0x1f                      ; mask field width (5 bits = 0x1f)
  ```

**Composite-init/access summary**:
| Construct | Layout/Storage | Codegen pattern |
|-----------|----------------|------------------|
| Nested struct | Flat (no padding inserted) | Direct offsets |
| Union | Shared, size = max(members) | Same offset for all members |
| Bitfields | Packed LSB-first in storage units | and+or for write; shr+and for read |

For the Rust reimplementation:
- Nested struct: emit as flat byte sequence
  without padding.
- Union: track member offsets all as 0; storage
  size = max.
- Bitfields: pack LSB-first; emit and/or for
  writes, shr+and for reads.

## Typedef-arr transparent; struct w/ ptr field init = FIXUPP to inline string; arr-of-struct flat layout

Fixtures `2099` (typedef array), `2100` (struct
with ptr field init), `2101` (array of struct)
exercise composite-type initializers.

- `2099` (**typedef array transparent**):
  ```c
  typedef int IntArr5[5];
  static IntArr5 a = {1,2,3,4,5};
  ```
  Byte-identical to `static int a[5] = {...}`.
  Typedef of array types is fully transparent at
  codegen.
- `2100` (**struct with `char *name`**): the
  string literal is placed in `_DATA` right
  after the struct, and the struct's ptr field
  is initialised via FIXUPP to point to it:
  ```
  ; _DATA layout:
  offset 0..1:  ptr_to_apple (FIXUPP, resolves to offset 4)
  offset 2..3:  qty (= 5)
  offset 4..9:  "apple\0"
  ```
  So `{"apple", 5}` emits the struct followed by
  the inline string literal. The ptr is a 2-byte
  near offset.
- `2101` (**array of struct flat layout**):
  ```c
  static struct P pts[3] = {{1,2}, {3,4}, {5,6}};
  ```
  Data emits `01 00 02 00 03 00 04 00 05 00 06
  00` (12 bytes for 3 structs × 4 bytes). Array
  stride = sizeof(struct) = 4. Field access:
  - pts[i].x at offset i*4
  - pts[i].y at offset i*4 + 2

**Composite-init summary**:
| Form | Layout |
|------|--------|
| `typedef T arr[N]; static T a[]=...;` | Same as direct array (transparent) |
| `static struct {char *p; int n;} s = {"x", 5}` | struct then inline string; ptr FIXUPP'd |
| `static struct S arr[N] = {...}` | N structs flat row-major; stride = sizeof(struct) |

For the Rust reimplementation:
- Track typedef'd array types; emit as primitive array.
- Struct with string literal field: emit struct bytes, then string, with FIXUPP for the ptr.
- Array of struct: emit struct-stride sequence.

## `char s[3]="abc"` drops `\0` (classic C trap); larger arr zero-pads; struct partial init zero-pads

Fixtures `2096` (char s[3]="abc"), `2097` (char
s[6]="ab"), `2098` (struct partial init) refine
the static-init rules.

- `2096` (**`char s[3] = "abc"` drops null**):
  data emits just `61 62 63` (3 bytes). The
  null terminator is **dropped** when the declared
  size matches the string length without room
  for it. **Classic C trap** — programmers who
  write `char s[3] = "abc"` lose the null.
- `2097` (**`char s[6] = "ab"` zero-pads
  including null**): data emits `61 62 00 00 00
  00` (6 bytes). "ab" + null at index 2 +
  three more zero bytes for padding.
- `2098` (**struct partial init**): `static
  struct S s = {10, 20};` (3-int struct) data
  emits `0a 00 14 00 00 00` (6 bytes). Like
  array partial init — unmentioned fields are
  **zero-padded**.

**Static-init filling summary**:
| Init form | Declared size | Result |
|-----------|---------------|--------|
| `int arr[N] = {a, b, ...}` (M ≤ N items) | N | first M filled, rest zero |
| `int arr[] = {a, b, ...}` | M | exactly M items |
| `char s[N] = "...str..."` (strlen+1 ≤ N) | N | string + null + zero-pad |
| `char s[N] = "...str..."` (strlen == N) | N | string only, **null dropped** |
| `char s[] = "...str..."` | strlen+1 | string + null |
| `struct S s = {a, b, ...}` | sizeof(struct) | first fields filled, rest zero |

For the Rust reimplementation:
- Static char-array init with exact-size: drop null.
- Static array partial init: zero-pad tail.
- Static struct partial init: zero-pad unmentioned
  fields.

## Partial arr init zero-pads tail; implicit size = init count; `char s[]="abc"` includes `\0`

Fixtures `2093` (partial int init), `2094`
(implicit size), `2095` (char arr from string)
cover array initializer semantics.

- `2093` (**`int arr[5] = {1, 2}`**): explicit
  values fill from index 0; remaining slots
  **zero-padded**. Data emits `01 00 02 00 00
  00 00 00 00 00` (10 bytes total — matches
  declared size, NOT init count).
- `2094` (**`int arr[] = {10, 20, 30, 40}`**):
  **implicit size = init count**. Data emits
  `0a 00 14 00 1e 00 28 00` (8 bytes for 4
  ints). Array sized to fit the explicit list.
- `2095` (**`char s[] = "abc"`**): sized to
  `strlen + 1` to include the **null terminator**.
  Data emits `61 62 63 00` (4 bytes for 3-char
  string).

**Array initializer summary**:
| Form | Sizing | Tail filling |
|------|--------|---------------|
| `int arr[5] = {1, 2}` | declared (5) | zero-pad remaining |
| `int arr[5] = {1, 2, 3, 4, 5}` | declared (5) | none (full) |
| `int arr[] = {1, 2, 3}` | init count (3) | n/a |
| `char s[] = "abc"` | strlen+1 (4) | n/a |
| `char s[3] = "abc"` | declared (3) | might TRUNCATE null! |
| `char s[10] = "abc"` | declared (10) | zero-pad tail |

For string literals as array initializers, the
**null terminator is implicitly included** when
size is implicit or sufficient.

For the Rust reimplementation:
- Track declared vs implicit array size.
- Emit explicit init values in order.
- For declared > init count: emit zero bytes for
  the remainder.
- String literal init: include `\0` if room
  remains.

## `x/D` and `x%D` NOT fused (2 idivs); `==0` mem uses `cmp [m], 0`; `== -1` uses imm8-sext

Fixtures `2090` (div + mod), `2091` (cmp == 0
mem), `2092` (cmp == -1 mem) reveal three
optimization points.

- `2090` (**div + mod NOT fused**): even with
  same divisor, BCC emits **two separate idiv
  instructions** — one for q, one for r:
  ```
  ; quotient:
  mov ax, [x] / mov bx, 7 / cwd / idiv bx
  mov [q], ax
  ; remainder:
  mov ax, [x] / mov bx, 7 / cwd / idiv bx
  mov [r], dx
  ```
  ~12 bytes of redundancy. Missed CSE — a single
  idiv produces both q (AX) and r (DX) at once.
  Confirms BCC's per-statement codegen (no CSE).
- `2091` (**`if (x == 0)` with memory operand**):
  emits `cmp word [bp+disp], 0` (`83 7e fe 00`,
  4 bytes). NOT `or reg, reg` since x is in
  memory, not a register. Compare to fixture
  [[2024]] where x was enregistered — that used
  the 2-byte `or si, si` form.
  
  Memory form: 4 bytes (`83 7e disp 00`).
  Register form: 2 bytes (`0b f6` for SI).
- `2092` (**`if (x == -1)` with memory**): emits
  `cmp word [bp+disp], -1` (`83 7e fe ff`, 4
  bytes). Uses the **imm8-sext form** (`83 /7`)
  since -1 fits as a sign-extended imm8 (0xFF).
  Avoids the longer `81 /7 reg imm16` (5 bytes).

**Zero/-1 comparison summary**:
| Operand | Encoding | Bytes |
|---------|----------|-------|
| `x == 0`, x in reg | `or reg, reg` + jcc | 2 + 2 |
| `x == 0`, x in [bp+disp] | `cmp word [bp+disp], 0` (imm8-sext) | 4 |
| `x == -1`, x in [bp+disp] | `cmp word [bp+disp], -1` (imm8-sext) | 4 |
| `x == imm16-not-fitting`, x in [bp+disp] | `cmp word [bp+disp], imm16` (`81 /7`) | 5 |
| `x == 0`, x in [global] | `cmp word [disp16], 0` (imm8-sext) | 5 |

For the Rust reimplementation:
- Don't try to fuse `x / D` and `x % D` — emit
  two separate idivs (matches BCC byte-for-byte).
- Choose cmp form based on operand storage:
  reg → `or`/`test`, mem (small const) →
  `83 /7`, mem (large const) → `81 /7`.

## Unsigned mod pow2 = `and ax, (N-1)`; signed mod = idiv with DX as result (NOT and-mask)

Fixtures `2087` (unsigned % 4), `2088` (signed %
4), `2089` (signed % 7) cover modulo codegen.

- `2087` (**`unsigned int % 4`**): emits **`25
  03 00`** (`and ax, 3`, AX-form imm16, 3 bytes).
  Uses the identity `x % 2^n = x & (2^n - 1)`.
- `2088` (**`int % 4` SIGNED**): does NOT use
  and-mask! Uses `idiv` and takes the remainder
  from DX:
  ```
  mov ax, [x]
  mov bx, 4
  cwd
  idiv bx
  mov [r], dx              ; remainder is in DX
  ```
  9 bytes for the modulo. Correct semantics —
  `-5 % 4 = -1` per C, but `-5 & 3 = 3`.
- `2089` (**`int % 7` signed**): same idiv
  pattern; divisor differs. Result from DX.

So the **only difference between `/` and `%`** is
whether you write AX (quotient) or DX
(remainder) after `idiv`.

**Division and modulo combined summary**:
| Operation | Quotient (`/`) | Remainder (`%`) |
|-----------|----------------|------------------|
| `unsigned / pow2`, `unsigned % pow2` | `shr` (logical) | `and reg, (N-1)` |
| `unsigned / non-pow2`, `unsigned % non-pow2` | `xor dx, dx / div bx` (q=AX) | `xor dx, dx / div bx` (r=DX) |
| `signed / any`, `signed % any` | `cwd / idiv bx` (q=AX) | `cwd / idiv bx` (r=DX) |

`int` (signed) ALWAYS uses idiv for both ops. Pow2
optimisation is **unsigned-only**.

**Useful trick — combining `/` and `%`**: when
both `x/4` and `x%4` are computed on the same x,
BCC could (in principle) compute them with a
single `idiv` (q in AX, r in DX). Not yet probed
whether BCC actually does this.

For the Rust reimplementation:
- `unsigned % pow2`: emit `and ax, N-1` (AX-form
  imm16, 3B).
- `signed %`: emit same as `signed /` but store
  DX instead of AX.

## Unsigned div pow2 = shr unrolled; signed div ALWAYS uses idiv (no shift, even for pow2)

Fixtures `2084` (unsigned div by 4), `2085`
(signed div by 4), `2086` (signed div by 7)
characterise division codegen.

- `2084` (**`unsigned int / 4`**): emits **2
  unrolled `shr ax, 1`** (`d1 e8 d1 e8`, 4
  bytes). Same threshold as left-shift: N ≤ 3
  unrolled, N ≥ 4 CL-form.
- `2085` (**`int / 4` SIGNED**): does **NOT**
  use shift! Uses `idiv bx`:
  ```
  mov ax, [x]
  mov bx, 4              ; bb 04 00 (divisor in BX)
  cwd                     ; 99 (sign-extend AX into DX:AX)
  idiv bx                 ; f7 fb (signed divide)
  ```
  6 bytes for the divide. **Correct semantics**:
  arithmetic right shift would round toward -∞
  for negative x (e.g., `-5 / 4 = -1` per C, but
  `-5 >> 2 = -2`).
- `2086` (**`int / 7` signed, non-pow2**): same
  `idiv bx` pattern. Divisor is 7 instead of 4
  — otherwise byte-identical to signed-div-by-4.

**Division codegen summary**:
| Operation | Encoding | Bytes |
|-----------|----------|-------|
| `unsigned / 2` | `shr ax, 1` | 2 |
| `unsigned / pow2-N (N ≤ 3)` | N× `shr ax, 1` | 2N |
| `unsigned / pow2-N (N ≥ 4)` | `mov cl, N / shr ax, cl` (CL-form) | 4 |
| `unsigned / non-pow2` | `mov bx, N / xor dx, dx / div bx` | (probably 6-7) |
| `int / any` (signed) | `mov bx, N / cwd / idiv bx` | 6 |

So **signed div always uses idiv** — no shift
optimization even for pow2. This is the correct
C semantics for negative dividends.

For the Rust reimplementation:
- `unsigned / pow2`: emit shift right (logical).
- `signed / anything`: emit `mov bx, divisor /
  cwd / idiv bx`.
- `unsigned / non-pow2`: emit `mov bx, divisor /
  xor dx, dx / div bx`.

## Confirmed: shift ≤3 = unrolled (NOT byte-optimal at N=3); shift ≥4 = CL-form

Fixtures `2081` (x * 8 = shift 3), `2082` (x * 2
= shift 1), `2083` (x * 32 = shift 5) pin the
shift-threshold rule.

- `2081` (**`x * 8` = shift by 3**): emits **3
  unrolled `shl ax, 1`** (`d1 e0 d1 e0 d1 e0`,
  6 bytes). NOT byte-count optimal — CL-form
  (`b1 03 d3 e0`) would be 4 bytes. BCC's rule
  is structural (≤3 unrolled), not size-driven.
- `2082` (**`x * 2` = shift by 1**): `d1 e0` (2
  bytes). Sanity check.
- `2083` (**`x * 32` = shift by 5**): `b1 05 d3
  e0` (4 bytes CL-form).

**Correct shift-by-N threshold rule**:
| Shift count N | Form | Bytes |
|----------------|------|-------|
| 1 | `shl ax, 1` (`d1 e0`) | 2 |
| 2 | `shl ax, 1 / shl ax, 1` | 4 |
| 3 | `shl ax, 1 / shl ax, 1 / shl ax, 1` | 6 (NOT optimal) |
| 4+ | `mov cl, N / shl ax, cl` (CL-form) | 4 |

So **N ≤ 3 unrolled** is the rule, even when N=3
costs an extra 2 bytes vs CL-form. The threshold
is purely structural — BCC doesn't optimise for
size in this specific case.

**Updated multiplication table** (shift-thresh
fixed):
| Multiplier | Encoding | Bytes |
|------------|----------|-------|
| 2 | `shl ax, 1` | 2 |
| 4 | 2× `shl ax, 1` | 4 |
| **8** | **3× `shl ax, 1` (6B, not CL-form)** | 6 |
| 16, 32, ... 32768 | CL-form | 4 |
| Non-pow2 | `mov dx, N / imul dx` | 5 |

For the Rust reimplementation:
- Shift by N: choose unrolled for N ≤ 3, CL-form
  for N ≥ 4 — regardless of byte-count tie-breaking.

## Pow-2 mul: shift ≤2 = unrolled `shl ax, 1`; shift ≥4 = CL-form; non-pow2 = `mov dx, N / imul dx`

Fixtures `2078` (x * 4), `2079` (x * 16), `2080`
(x * 7) characterise multiplication codegen.

- `2078` (**`x * 4` = shift by 2**): emits **2
  `shl ax, 1`** (`d1 e0 d1 e0`, 4 bytes). Same
  byte count as CL-form (`b1 02 d3 e0`), but
  BCC picks unrolled for N=2.
- `2079` (**`x * 16` = shift by 4**): emits
  **CL-form**: `b1 04 d3 e0` (4 bytes). With N=4,
  unrolled would be 8 bytes — CL-form wins
  decisively.
- `2080` (**`x * 7` non-pow2**): emits **`mov
  dx, N / imul dx`** (`ba 07 00 f7 ea`, 5
  bytes):
  ```
  mov ax, [x]
  mov dx, 7              ; ba 07 00 (load multiplier)
  imul dx                 ; f7 ea (signed multiply, AX *= DX)
  ```
  Result in AX (low half) — for 16-bit int the
  high half (DX) is discarded.

**Multiplication-encoding rule**:
| Multiplier | Encoding | Bytes |
|------------|----------|-------|
| 0 | `mov ax, 0` or `xor ax, ax` (identity-fold) | 2-3 |
| 1 | `mov ax, x` only (identity-fold) | 0 (no mul op) |
| 2 | `shl ax, 1` (`d1 e0`) | 2 |
| 4 | `shl ax, 1 / shl ax, 1` (unrolled) | 4 |
| 8 | (probably) `mov cl, 3 / shl ax, cl` CL-form | 4 |
| 16, 32, ... 32768 | CL-form `mov cl, N / shl ax, cl` | 4 |
| Non-pow2 | `mov dx, N / imul dx` | 5 |

**Shift-by-N threshold**: same as the general shift
rule:
- shift ≤ 2 → unrolled `shl ax, 1`
- shift ≥ 3 → CL-form `mov cl, N / shl ax, cl`

For the Rust reimplementation:
- Mul by pow2: detect pow2 multiplier, emit shift.
- Mul by 1: identity-fold (just load).
- Mul by 0: zero-fold (direct store 0).
- Mul by non-pow2: `mov dx, multiplier / imul dx`.

## Const-combined +5-5 emits `add ax, 0` (NOT folded back to identity); +1+1+1 = single add 3

Fixtures `2075` (x + 1 + 1 + 1), `2076` (x + 5 -
2), `2077` (x + 5 - 5) confirm const-combination
behaviour and reveal an **optimization gap**.

- `2075` (**`x + 1 + 1 + 1` = `x + 3`**): same
  output as `x + 3`. Const-combination across
  multiple +1's:
  ```
  mov ax, [x]
  add ax, 3                 ; single add of folded constant
  ```
- `2076` (**`x + 5 - 2` = `x + 3`**): mixed
  add/sub of constants folds to net (`+3`). Same
  output as `x + 3`.
- `2077` (**`x + 5 - 5`**): combines to `x +
  0`, but BCC **emits `add ax, 0`** (3-byte
  no-op) instead of identity-folding it away!
  ```
  mov ax, [x]
  05 00 00                  ; add ax, 0 (no-op, NOT eliminated)
  ```
  
  **Optimization gap**: identity-fold for `+ 0`
  only triggers on a LITERAL 0 in the source. If
  the constant-combination phase produces 0,
  the result is NOT re-fed into the identity
  check.

So BCC's optimization order is:
1. Parse expression
2. Const-combine adjacent constants
3. Check for literal-identity ONLY for the
   original source literals — not for combined
   results

This is consistent with BCC's simple single-pass
strategy.

For the Rust reimplementation:
- Const-combine first (folds adjacent constants).
- Re-check identity-folds AFTER const-combination
  to catch the `x + 5 - 5` case. (Note: this
  would NOT match BCC byte-for-byte; to match,
  we must emit the redundant `add 0`.)
- Alternative: implement only the parse-time
  literal identity check (matches BCC).

## Small-add asymmetry: `x+1`/`+2` = inc, `x-1` = dec, but `x-2` = `add ax, -2` (NOT dec dec)

Fixtures `2072` (x + 3), `2073` (x - 1), `2074`
(x - 2) refine the small-constant add/sub
encoding rule.

- `2072` (**`x + 3`**): emits **`05 03 00`** —
  `add ax, 3` in AX-form imm16 (3 bytes). NOT
  `83 c0 03` (modrm imm8-sext, also 3 bytes) and
  NOT 3 incs (3 bytes). BCC picks the AX-form
  for AX.
- `2073` (**`x - 1`**): emits **`48`** — `dec
  ax` (1 byte). Mirrors `x + 1` → `inc ax`.
- `2074` (**`x - 2`**): emits **`05 fe ff`** —
  `add ax, -2` (= 0xFFFE) (3 bytes, AX-form
  imm16). **NOT `dec ax / dec ax` (2 bytes)!**
  BCC misses this optimization — sub by 2 goes
  through the general add-with-negated-constant
  path.

**Refined small-add/sub encoding rule** (corrected):
| Operation | Encoding | Bytes |
|-----------|----------|-------|
| `x + 1` | `inc ax` (`40`) | 1 |
| `x + 2` | `inc ax / inc ax` (`40 40`) | 2 |
| `x + 3` to `x + 127` | `add ax, imm16` AX-form (`05 imm16`) | 3 |
| `x + 128` to `x + 65535` | `add ax, imm16` AX-form (`05 imm16`) | 3 |
| `x - 1` | `dec ax` (`48`) | 1 |
| `x - 2` and above | `add ax, -N` AX-form (`05 imm16`) | 3 |

So the optimization is **asymmetric**:
- inc/dec only for ±1
- inc inc only for +2 (NOT dec dec for -2)
- otherwise AX-form `05 imm16` add

For the Rust reimplementation:
- `x + 1` → `40` (1B)
- `x + 2` → `40 40` (2B)
- `x - 1` → `48` (1B)
- All other small-const add/sub on AX → `05 imm16` (3B, AX-form)
- Note: BCC does NOT use the imm8-sext modrm form (`83 /0` or `83 /5`) for AX even when imm fits — always prefers AX-form.

## `register` enregisters into SI/DI; typedef = type-only no codegen; enum = parse-time int consts

Fixtures `2069` (register), `2070` (typedef),
`2071` (enum) finish the type-system survey.

- `2069` (**`register int`**): two `register
  int` declarations get enregistered into SI
  and DI:
  ```
  be 05 00                ; mov si, 5 (x in SI)
  bf 0a 00                ; mov di, 10 (y in DI)
  mov ax, si / add ax, di
  ```
  For simple cases (≥2 reads, ≤2 register-eligible
  locals), BCC would enregister anyway, so the
  effect is mostly a hint. May make a difference
  with many candidate locals.
- `2070` (**`typedef`**): byte-identical to using
  the base type directly. **Purely type-system**;
  no codegen effect. `typedef int mytype; mytype
  x = 10;` ≡ `int x = 10;`.
- `2071` (**`enum`**): enum values are **parse-
  time integer constants** (like `#define` but
  type-checked):
  - `RED` = 0, `GREEN` = 1, `BLUE` = 2 by default
  - `enum color c = GREEN` stores `1` (2-byte int)
  - `c * 10 + RED + BLUE` folds the constants
    `0 + 2 → 2` at parse time
  - Result emits as `c * 10 + 2` with `inc ax /
    inc ax` (`40 40`, 2 bytes) for the +2.

**Small-constant add optimisation**:
| Adjust | Encoding | Bytes |
|--------|----------|-------|
| +1 | `inc reg` | 1 |
| +2 | `inc reg / inc reg` | 2 |
| +3 to +127 | `add reg, imm8` (sext) | 3 |
| +128 to +32767 | `add reg, imm16` (AX) or `add reg, imm16` (mod-rm) | 3 |

So `inc reg` is preferred for +1/+2 over `add
reg, imm8` (3 bytes).

**Type-keyword summary**:
| Keyword | Effect |
|---------|--------|
| `register` | Hint for enregistration |
| `typedef` | Type alias (no codegen) |
| `enum` | Parse-time int consts (no runtime tag) |
| `const` | Type qualifier (no codegen) |
| `volatile` | Type qualifier (no codegen at -O0) |
| `extern` | Symbol declaration (no definition emitted) |

For the Rust reimplementation:
- `register`: pass to register allocator as a
  preference hint.
- `typedef`: resolve to underlying type at parse.
- `enum`: emit values as int constants; no enum
  type information in the OBJ.
- Small-const adds: prefer `inc reg` × N for
  N ≤ 2.

## `interrupt` = save all regs + load DS + IRET; `volatile`/`const` no codegen diff in trivial cases

Fixtures `2066` (interrupt fn), `2067` (volatile),
`2068` (const) explore three special qualifiers.

- `2066` (**`interrupt` keyword**): emits the
  canonical DOS ISR shape — full register save,
  reload DS to DGROUP, IRET:
  ```
  ; _my_isr:
  push ax / push bx / push cx / push dx    ; 50 53 51 52
  push es / push ds                          ; 06 1e
  push si / push di / push bp                ; 56 57 55
  mov bp, segment_of_DGROUP                  ; bd [seg] [seg] (FIXUPP)
  mov ds, bp                                  ; 8e dd (reload DS)
  mov bp, sp                                  ; 8b ec (frame, AFTER ds load)
  ; ...body...
  pop bp / pop di / pop si                   ; 5d 5f 5e
  pop ds / pop es                             ; 1f 07
  pop dx / pop cx / pop bx / pop ax           ; 5a 59 5b 58
  iret                                        ; cf
  ```
  Total prologue: 16 bytes; epilogue: 13 bytes.
  Uses **`iret`** (`cf`, 1 byte) which pops
  flags + cs + ip (vs `ret` / `retf`).
- `2067` (**`volatile`**): in the trivial case
  `v = 1; v = 2; return v;`, BCC emits both
  stores then a load. **Byte-identical to
  non-volatile** for this case — because BCC
  already doesn't do DCE/CSE. Volatile only
  shows up if BCC would otherwise optimise
  (which it rarely does). Probably a no-op in
  most cases.
- `2068` (**`const`**): `return c;` for `const
  int c = 42;` emits a runtime **load** (`a1 00
  00` with FIXUPP), NOT inline-fold to `mov ax,
  42`. **`const` is a type qualifier only** —
  doesn't enable parse-time const propagation.
  
  Compare:
  - `return 42` → `b8 2a 00` (mov ax, 42)
  - `return c` (with const c=42) → `a1 00 00` (load from memory)

**Type qualifier summary**:
| Qualifier | Codegen effect | Note |
|-----------|----------------|------|
| `const` | None at OBJ level | Type-system only |
| `volatile` | None (BCC doesn't DCE/CSE anyway) | Defensive |
| `register` | Hint for enregistration (when possible) | Discretionary |
| `static` | Local lifetime → `_DATA` placement | Storage class |
| `extern` | Declares, doesn't define | Symbol-table |

**Calling-convention keyword summary** (updated):
| Keyword | Effect |
|---------|--------|
| `cdecl` | Default — R-to-L args, caller cleans, `_name` |
| `pascal` | L-to-R args, callee cleans (`ret imm16`), `NAME` |
| `near` | Force near call/ret (`c3`) |
| `far` | Force far call/ret (`cb`, `[bp+6]`) |
| `interrupt` | Full reg save + ds reload + IRET |

For the Rust reimplementation:
- `interrupt`: emit the full ISR prologue/epilogue;
  no normal `push bp / mov bp, sp` (BP saved later).
- `volatile`/`const`: type-system tracking only;
  no codegen difference for current optimisation
  level.

## Pascal 4-args = `ret 8`; `cdecl` keyword = default; pascal→pascal call needs no cleanup

Fixtures `2063` (pascal 4 args), `2064` (cdecl
explicit), `2065` (pascal→pascal) complete the
calling-convention picture.

- `2063` (**pascal with 4 args = `ret 8`**):
  callee body:
  ```
  ; SUM4:
  mov ax, [bp+10]         ; a (first pushed, highest)
  add ax, [bp+8]          ; b
  add ax, [bp+6]          ; c
  add ax, [bp+4]          ; d
  pop bp / c2 08 00        ; ret 8 (= 4 args × 2)
  ```
  Caller pushes 1, 2, 3, 4 in L-to-R order; no
  cleanup. **Callee always cleans regardless of
  arg count**.
- `2064` (**`cdecl` keyword = default**):
  byte-identical output to omitting the keyword.
  Symbol `_helper` (with underscore), `c3` near
  ret, caller cleanup. Just an explicit
  affirmation.
- `2065` (**pascal calls pascal**): both fns use
  pascal convention. The caller (OUTER) pushes
  via `ff 76 04` (push word [bp+4]) — no
  intermediate load. Then `e8` call near, no
  cleanup. INNER returns with `ret 2`.
  ```
  ; OUTER (pascal):
  push bp / mov bp, sp
  push word [bp+4]         ; ff 76 04 — y arg
  call INNER               ; e8 ea ff
  ; (no cleanup — INNER did c2 02 00)
  shl ax, 1                 ; y * 2
  pop bp / c2 02 00         ; ret 2 (OUTER cleans for its caller)
  ```
  Main (default cdecl) calls OUTER same way (no
  cleanup since OUTER cleans).

**Calling-convention summary, complete**:
| Convention | Args | Cleanup | Naming | Keyword |
|-----------|------|---------|--------|---------|
| cdecl (default) | R-to-L | Caller (post-call cleanup) | `_name` | `cdecl` (explicit) or omit |
| pascal | L-to-R | Callee (`ret imm16`) | `NAME` (UPPER, no `_`) | `pascal` |
| `near` modifier | (preserves convention) | (preserves) | (preserves) | `near` |
| `far` modifier | (preserves convention) | (preserves) | (preserves) | `far` |

For the Rust reimplementation:
- `cdecl` keyword: same codegen as default.
- pascal `ret imm16`: `c2 [imm16]`, total cleanup bytes = N_args × 2.
- Pascal-to-pascal calls: omit caller cleanup.
- Mixing conventions in same file is fine; each fn
  follows its declared convention.

## `far fn` in small / `near fn` in medium: per-fn override of model default; `pascal` = L-to-R + callee-clean + UPPER

Fixtures `2060` (`int far helper` in small),
`2061` (`int near helper` in medium), `2062`
(`pascal` calling convention) explore per-function
overrides and alternative calling conventions.

- `2060` (**`int far helper(...)` in -ms**): the
  function-level `far` keyword **promotes** one
  function to far while leaving others as model
  default:
  ```
  ; _helper:
  push bp / mov bp, sp
  mov ax, [bp+6]                 ; arg shifts to +6 (far ret = 4B)
  inc ax
  pop bp / cb                     ; retf
  
  ; _main calling _helper (intra-segment since same _TEXT):
  push 41
  0e                              ; push cs
  e8 ea ff                        ; call near _helper
  ```
  Main itself stays NEAR (returns `c3`).
- `2061` (**`int near helper(...)` in -mm**):
  the `near` keyword **demotes** a function in
  medium/large to near, saving the push cs:
  ```
  ; _helper:
  push bp / mov bp, sp
  mov ax, [bp+4]                  ; arg at +4 (near ret = 2B)
  inc ax
  pop bp / c3                     ; near ret
  
  ; _main calling _helper (no push cs needed):
  push 41
  e8 eb ff                        ; call near _helper
  ```
  Main itself stays FAR in medium model (returns `cb`).
- `2062` (**`pascal` calling convention**): three
  major differences from cdecl:
  1. **Symbol name UPPERCASE, NO leading underscore**:
     `HELPER` (not `_helper`).
  2. **Args pushed LEFT-to-RIGHT**: in source
     order `helper(50, 8)` → push 50 first, push 8
     second. So 'a' is at [bp+6] (pushed first =
     higher address), 'b' at [bp+4].
  3. **Callee cleans the stack** via `ret imm16`:
     `c2 04 00` = `ret 4` (pops 4 args bytes).
     Caller does NO cleanup.
  
  ```
  ; HELPER:
  push bp / mov bp, sp
  mov ax, [bp+6]                  ; a (first pushed)
  sub ax, [bp+4]                  ; b
  pop bp / c2 04 00               ; ret 4 (callee cleans)
  
  ; _main calling HELPER:
  push 50                          ; a, L-to-R
  push 8                           ; b
  e8 e3 ff                         ; call HELPER
  ; NO cleanup — callee did it
  ```

**Function-keyword/convention summary**:
| Keyword/conv | Effect | Args | Cleanup | Naming |
|--------------|--------|------|---------|--------|
| (default cdecl) | per-model default | R-to-L | caller | `_name` |
| `near` | force near (model overrides irrelevant in -ms/-mc) | R-to-L | caller | `_name` |
| `far` | force far (use `cb` retf, shift offsets) | R-to-L | caller | `_name` |
| `pascal` | force pascal convention | L-to-R | callee (`ret imm16`) | `NAME` (UPPER, no underscore) |

For the Rust reimplementation:
- Per-fn `near`/`far` keywords: track at parse time,
  generate the correct call/ret pair.
- `pascal` convention: emit args L-to-R, use `ret
  imm16` in callee, use uppercase no-underscore
  symbol names.

## Huge model = far data + `push ds/mov ds,seg/.../pop ds`; `far *` ptr = LES BX; `near *` redundant in -ms

Fixtures `2057` (huge -mh), `2058` (explicit `far
*` in small), `2059` (explicit `near *` in small)
explore far data and the explicit far/near
keywords.

- `2057` (**huge model -mh**): segment names
  include `HELLO_TEXT`, `HELLO_DATA`, **`FAR_DATA`**
  — confirming far data. Each access loads DS:
  ```
  1e                      ; push ds (save)
  b8 [seg] [seg]          ; mov ax, segment_of_g (FIXUPP type=segment)
  8e d8                   ; mov ds, ax
  a1 [off] [off]          ; mov ax, [g] (FIXUPP for offset)
  1f                      ; pop ds (restore)
  ```
  **11 bytes** for one data access — expensive!
- `2058` (**`int far *p` in small model**): the
  far ptr is **4 bytes** (offset + segment):
  ```
  8c 5e fe                ; mov [bp-2], ds (high half = segment from DS)
  c7 46 fc 00 00          ; mov [bp-4], 0 (FIXUPP for offset, low half)
  ```
  Deref via LES:
  ```
  c4 5e fc                ; les bx, [bp-4]  (offset→bx, seg→es)
  26 8b 07                ; mov ax, ES:[bx]
  ```
  The `c4 /r` (LES) instruction is the canonical
  far-ptr load.
- `2059` (**`int near *p` in small model**):
  **byte-identical to default** (no near keyword)
  — `near` is a no-op in small model:
  ```
  be 00 00                ; mov si, 0 (FIXUPP for offset)
  8b 04                   ; mov ax, [si]
  ```

**Far/near pointer summary**:
| Type | Size | Construction | Deref |
|------|------|--------------|-------|
| `int *` (small-model default) | 2B | mov ax, offset (FIXUPP) | `8b /r` or `a1` |
| `int near *` (small) | 2B | (same as default) | (same) |
| `int far *` | 4B | mov [high], ds + mov [low], offset (FIXUPP) | `c4 /r` (les) then `26 8b /r` |
| `int *` in huge model | 4B (implicit far) | same as far | same as far |

For the Rust reimplementation:
- Far ptr type: track 4-byte representation.
- Far ptr load (construct): `mov [high], ds` (if from local DGROUP) + `mov [low], offset` with FIXUPP.
- Far ptr deref: emit LES (`c4 /r`) then segment-override prefixed mov (`26 8b /r`).
- Huge model data access: emit the full `push ds / mov ds, seg / ... / pop ds` envelope around the access.

## Medium model: intra-seg call = `push cs / call near`; arg at `[bp+6]`; retf `cb`; data still near

Fixtures `2054` (medium fn call), `2055` (medium
recursion), `2056` (medium string arg) reveal
medium model's call/return shapes.

- `2054` (**intra-segment call = `0e e8 [rel]`**):
  ```
  push 41                         ; arg
  0e                              ; push cs (1B)
  e8 ea ff                        ; call near _helper (3B)
  pop cx                           ; cleanup
  ```
  Total 4 bytes for the call + push cs, vs 5
  bytes for full CALL FAR (`9a [off] [seg]` with
  FIXUPP). Since caller and callee are in the
  **same code segment**, push cs gives the
  correct segment for the eventual `retf`.
  
  The callee returns with `5d cb` (pop bp / retf)
  which pops both offset and segment.
- `2055` (**recursive intra-seg call**): same
  `0e e8 [rel]` pattern for the recursive call.
  ```
  push (n-1)
  0e
  e8 e7 ff                        ; call near _fact (recursive)
  pop cx
  imul si                          ; * n
  ```
  Optimization applies to any intra-segment call
  in medium/large models.
- `2056` (**string arg from `_DATA`**): string
  literal still in `_DATA` (DGROUP near). Push as
  2-byte near offset with FIXUPP. Same as small
  model:
  ```
  mov ax, 0                        ; b8 00 00 (FIXUPP)
  push ax
  0e
  e8 d8 ff                        ; call near _strlen_local
  ```

**Medium-model stack frame** (and large model):
```
[bp+0]: saved BP
[bp+2]: return offset
[bp+4]: return segment           <-- extra 2 bytes
[bp+6]: first arg
```
Args start at `[bp+6]` instead of `[bp+4]` due
to the far return address.

For the Rust reimplementation:
- Medium/large code: emit `push cs / call near`
  for intra-segment calls (avoids the FIXUPP
  segment field).
- Use `retf` (`cb`) for function returns in
  medium/large.
- Arg offsets: start at `[bp+6]` in medium/large.

## Memory-model byte-level diffs: near vs far code (`c3` vs `cb` ret); code-seg name per-file in mm/ml

**First pivot away from small-only**: fixtures
`2051` (compact -mc), `2052` (medium -mm), `2053`
(large -ml) all compile **the same source**
`int g = 42; int main() { return g; }`. The
differences are the **memory model**.

- `2051` (**compact, -mc**): **identical to small**
  for this trivial source. Single `_TEXT` segment,
  `5d c3` (pop bp / near ret). Data in `_DATA` via
  DGROUP, access by `a1 disp16`.
- `2052` (**medium, -mm**): code segment renamed
  to **`HELLO_TEXT`** (= per-file `<fname>_TEXT`).
  Return is **`5d cb`** (pop bp / **far ret**).
  Data still `a1 disp16` via DGROUP (near data).
- `2053` (**large, -ml**): byte-identical to
  medium for this trivial test — `HELLO_TEXT` for
  code, `5d cb` for far ret, `a1 disp16` for
  data.

**Memory-model summary** (from these probes):
| Model | Code | Data (trivial) | Code seg | RET |
|-------|------|----------------|----------|-----|
| small (-ms) | near | near | `_TEXT` | `c3` |
| compact (-mc) | near | near | `_TEXT` | `c3` |
| medium (-mm) | far | near | `<fname>_TEXT` | `cb` |
| large (-ml) | far | near | `<fname>_TEXT` | `cb` |
| huge (-mh) | far | far | (not yet probed) | (not yet probed) |

So the two key bits visible in OBJ output:
1. **Code segment naming**: small/compact use the
   single global `_TEXT`; medium/large use a
   per-source-file `<fname>_TEXT`.
2. **Function return**: small/compact use `c3`
   (near ret 0xC3); medium/large use `cb` (far
   ret 0xCB).

Data access in the trivial case is identical (`a1
disp16`) for all four because the global g lives
in DGROUP'd `_DATA`. To distinguish compact/large
from small/medium, we'd need multi-segment data or
explicit `far` data — not yet probed.

For the Rust reimplementation:
- Parse model from `-ms` / `-mc` / `-mm` / `-ml`.
- Code segment: emit `_TEXT` for s/c, `<fname>_TEXT` for m/l.
- Function epilogue: emit `c3` for s/c near ret, `cb` for m/l far ret.
- Data: keep DGROUP near in all base cases.

## Static-no-init in `_DATA` zero-fill; empty stmts emit nothing; binary ops eval RIGHT-to-left

Fixtures `2048` (static no init), `2049` (empty
stmts), `2050` (3 trivial fns + composed expr)
cover three further idioms.

- `2048` (**static int arr no init**): placed in
  `_DATA` with **size 6 bytes, NO LEDATA** — the
  segment is zero-filled by default. SEGDEF
  declares the length; loader provides the zero
  bytes. No init bytes emitted.
  
  Access via direct addressing with FIXUPP
  (same as initialised statics).
- `2049` (**empty statements emit nothing**):
  `;` `;` `;` produce **zero bytes** in the
  output. They're skipped at parse time.
- `2050` (**right-to-left binary op eval**):
  `zero() + one() * neg_one()` parses as `zero()
  + (one() * neg_one())`. Codegen order:
  1. **neg_one() first** (rightmost) → AX = -1
  2. push AX (save)
  3. **one()** → AX = 1
  4. mov dx, ax
  5. pop ax (= -1 from neg_one)
  6. imul dx → AX = -1*1 = -1
  7. push -1 (save the * result)
  8. **zero()** (leftmost) → AX = 0
  9. pop dx (= -1)
  10. add ax, dx → -1
  
  So **binary operators evaluate RHS first**, then
  LHS, consistent with cdecl R-to-L for fn args.
  
  Also notable: `_zero` body uses `xor ax, ax`
  (2 bytes) for returning 0; `_one`/`_neg_one`
  use `mov ax, imm16` (3 bytes). So **only the
  literal 0 gets the xor optimisation**; -1, 1,
  etc. use the standard mov.

**Order-of-eval summary**:
| Construct | Eval order |
|-----------|------------|
| Fn args | Right-to-left (matches push order) |
| Binary operator operands (`a + b`, etc.) | Right-to-left |
| Comma operator (`a, b`) | Left-to-right (C standard, sequence point) |
| && / || | Left-to-right (short-circuit) |

So **side-effects within binary ops are observable RIGHT-first**, which surprises programmers used to GCC's left-first cdecl convention.

For the Rust reimplementation:
- Static no init: emit SEGDEF with the size; no LEDATA bytes (zero-filled).
- Empty stmts: emit nothing.
- Binary ops: evaluate RHS first, then LHS.
- Constant 0 in expressions: emit `xor ax, ax`; other constants use `mov ax, imm16`.

## `while(--x)` = `dec/jne` (no cmp); arr decays via `lea`; static int arr = `_DATA` init list

Fixtures `2045` (while predec), `2046` (arr decay
in fn call), `2047` (static int arr init) cover
three idioms.

- `2045` (**`while (--x)` = `dec / jne` only**):
  the dec instruction sets ZF; no separate cmp
  needed:
  ```
  jmp test
  body:
    inc si               ; count++
  test:
    dec di                ; --x (sets flags)
    jne body              ; loop while result != 0
  ```
  3 bytes for test+update (`4f / 75 fc`). For x
  = 5: 4 iterations (dec to 4,3,2,1 — all
  non-zero), exit on dec to 0.
- `2046` (**array decay = lea + push**):
  ```
  lea ax, [bp-6]           ; address of arr[0]
  push ax                   ; push the address
  ```
  Array name in expression context decays to
  pointer (= address of first element). `lea`
  computes the effective address; `push ax`
  pushes it.
- `2047` (**static int arr with init list**):
  values emitted in `_DATA` in order (`0a 00 14
  00 1e 00` = 10, 20, 30 little-endian).
  Access uses direct addressing:
  ```
  mov ax, [arr[0]]         ; a1 disp16 (FIXUPP)
  add ax, [arr[1]]         ; 03 06 disp16
  add ax, [arr[2]]         ; 03 06 disp16
  ```
  Static globals/locals live in `_DATA` and use
  the AX-form `a1`/`a3` for load/store (3 bytes)
  and the modrm-form `03 06 disp16` for add (4
  bytes).

For the Rust reimplementation:
- `while (--x)`: emit `dec / jne` directly (no
  preceding cmp).
- Array decay in call/expression: emit `lea` to
  compute address, then push.
- Static arr with init list: emit `_DATA` bytes
  in order; FIXUPP each access.

## `fn(a[i])` push via stack slot directly; `a[i] = fn()` stores AX; cmp with fn result swaps operands

Fixtures `2042` (arr elem as fn arg), `2043` (arr
store from fn), `2044` (cmp against fn result)
cover three idioms involving fn calls + memory.

- `2042` (**`dbl(a[1])` push direct**): for
  constant-indexed array element as fn arg, BCC
  pushes directly via the stack slot:
  ```
  ff 76 fc                ; push word [a[1]]  (= [bp-4])
  call _dbl
  pop
  ```
  3 bytes for the push — no intermediate
  load+push.
- `2043` (**`a[i] = fn(val)` store result**):
  ```
  mov ax, val / push ax
  call _square
  pop
  mov [a[i]], ax           ; store return value directly
  ```
  Standard pattern: result lives in AX after the
  call; store to the array slot.
- `2044` (**`if (x > fn())` cmp with swapped
  operands**): the call result lives in AX; BCC
  swaps the cmp operands to fit:
  ```
  call _get_threshold
  cmp ax, [x]             ; reversed operand order (ax as dest)
  jge L_false             ; jge for the reversed sense
  ```
  Since cmp's operands are swapped (`ax - [x]`
  instead of `[x] - ax`), the inverse-jcc is
  `jge` (instead of `jle` if we'd written `cmp
  [x], ax`).
  
  Same swap trick as the pointer-walk loop at
  [[1814-pointer-walk]] — saves bytes by using
  the AX-form cmp.

For the Rust reimplementation:
- `fn(arr[i])`: emit `push [arr+offset]` directly,
  no intermediate load.
- `arr[i] = fn()`: call then store AX to the
  array slot.
- cmp with fn result: prefer cmp with ax as
  reg-field (swapping operands), adjust jcc
  accordingly.

## Infinite loops `while(1)`/`for(;;)`/`do-while(1)` = body + jmp top; no test emitted

Fixtures `2039` (while(1)), `2040` (for(;;)),
`2041` (do-while(1)) confirm infinite-loop
codegen.

- `2039` (**`while (1) { body }`**): no test
  emitted at the top. Just body + unconditional
  `jmp` back:
  ```
  L_top:
    body
    jmp L_top                ; eb f6 (no test)
  L_break:
  ```
- `2040` (**`for (i=0;; i++) { body }`**): empty
  cond treated as always-true. Init runs first,
  then loop: body + update + jmp top.
- `2041` (**`do {body} while (1)`**): **byte-
  identical** to `while (1) {body}` — both
  compile to body + jmp top.

**Infinite-loop codegen summary**:
| Construct | Codegen |
|-----------|---------|
| `while (1) {body}` | `L_top:` body + `jmp L_top` |
| `do {body} while (1)` | (same — byte-identical) |
| `for (init;; update) {body}` | init + `L_top:` body + update + `jmp L_top` |
| `for (;;) {body}` | `L_top:` body + `jmp L_top` |
| `while (literal-non-zero N) {body}` | (probably same as while(1)) |

Always-true conditions elide the test entirely.
The cmp/jcc is eliminated at parse time.

For the Rust reimplementation:
- Detect literal-non-zero conditions in if/while/
  do/for; elide the test instruction.
- Always emit `jmp top` for the back-edge.

## Cleanup is byte-based not arg-based: long-arg (4B) uses pops; int+long (6B) uses add sp

Fixtures `2036` (3 int args = 6B), `2037` (long
arg = 4B), `2038` (int+long = 6B) refine the
cleanup encoding rule.

- `2036` (**3 args = 6B = add sp, 6**): `add sp,
  6` (3 bytes via imm8-sext). Confirms ≥6 bytes
  triggers the add-sp form.
- `2037` (**long arg = 4B = pop cx × 2**): a
  single long arg pushes 4 bytes (high then low
  half). Cleanup uses `pop cx × 2` (2 bytes) —
  same as 2 int args. **Bytes, not args, determine
  the cleanup form**.
- `2038` (**int + long = 6B = add sp, 6**):
  total bytes pushed = 2 + 4 = 6. Cleanup uses
  add sp, 6. Confirms the byte-count rule.

**Refined cleanup encoding rule** (by total bytes
pushed):
| Total bytes pushed | Cleanup | Bytes used |
|---------------------|---------|------------|
| 0 | (none) | 0 |
| 2 | `pop cx` | 1 |
| 4 | `pop cx × 2` | 2 |
| ≥6 | `add sp, N` (imm8-sext or imm16) | 3-4 |

So 0/2/4 bytes use the `pop cx` form (cheap for
small cleanups); 6+ bytes uses `add sp, N` (constant
3-byte form). The threshold is byte-count, not
arg-count — making it work uniformly for mixed
int/long args.

For the Rust reimplementation:
- Compute total push bytes for the call site.
- Choose cleanup form per the table above.
- Use `pop cx` (not `pop ax`) to preserve the
  return value in AX.

## Cleanup encoding: 0 args = no cleanup; 1-2 args = `pop cx` (preserves AX); 3+ = `add sp, N`

Fixtures `2033` (0 args), `2034` (1 arg), `2035`
(2 args) characterise the **post-call cleanup**
encoding.

- `2033` (**0 args = no cleanup**): just `call`
  + `ret`. No `pop` or `add sp` emitted.
- `2034` (**1 arg = `pop cx`**): 2-byte cleanup
  via single `pop cx` (`59`, 1 byte).
  - Critically uses **`pop cx`** (`59`), NOT
    `pop ax` (`58`) — preserves AX which holds
    the return value. CX is caller-saved so
    clobbering is fine.
- `2035` (**2 args = `pop cx` × 2**): 4-byte
  cleanup via two `pop cx` instructions (2
  bytes total). Cheaper than `add sp, 4` (3
  bytes).

**Cleanup encoding hierarchy**:
| N args | Cleanup | Bytes |
|--------|---------|-------|
| 0 | (none) | 0 |
| 1 | `pop cx` | 1 |
| 2 | `pop cx / pop cx` | 2 |
| 3 | `add sp, 6` (imm8-sext) | 3 |
| 4+ | `add sp, N*2` (imm8-sext or imm16) | 3-4 |

So the boundary is at N=3 args, where pops (3 bytes
for 3 pops) and add-sp (3 bytes) are tied — BCC
picks `add sp` for clarity/consistency.

For the Rust reimplementation:
- 0 args: omit cleanup
- 1-2 args: emit `59` (pop cx) per arg
- 3+ args: emit `83 c4 imm8` (add sp, N*2)

## 3-arg eval order confirmed R-to-L; shift+mask no fusion; int overflow wraps naturally

Fixtures `2030` (3-arg side effects), `2031` (bit
extract), `2032` (int overflow) cover three more
patterns.

- `2030` (**3-arg side-effect order = R-to-L
  confirmed**): `sum3(log(1), log(2), log(3))`
  observes log(3) first, log(2), log(1) last.
  Side-effect-order matches push-order (cdecl
  R-to-L). With 3 calls and 2-digit-traces:
  order builds as 3 → 32 → 321. Final order =
  321.
- `2031` (**bit extract via shift+mask**): `(x >>
  8) & 0x0F`:
  ```
  mov ax, [x]
  mov cl, 8 / shr ax, cl       ; shift
  and ax, 0x0F                  ; mask (AX-form imm16)
  ```
  No fusion. Standard sequence.
- `2032` (**int overflow wraps**): `30000 + 5000
  = 35000` (wraps to -30536 in signed int, or
  35000 in unsigned). BCC emits standard `add`
  — no overflow check, just modulo-65536
  arithmetic.
  
  C89 says signed overflow is UB; BCC's
  behavior is "just do the add, wrap silently."

For the Rust reimplementation:
- N-arg side effects: emit args R-to-L (each call
  emits the inner subexpression, then push).
- Shift+mask: no special fusion; emit each
  operation independently.
- int arithmetic: no overflow checks; let it wrap.

## String arg = FIXUPP offset push; sizeof types parse-time const; `return (a,b)` yields b

Fixtures `2027` (string arg), `2028` (sizeof
various types), `2029` (comma in return) cover
three remaining idioms.

- `2027` (**string arg to fn**): passing a string
  literal:
  ```
  mov ax, 0              ; b8 00 00 (with FIXUPP to string)
  push ax
  call _len_to_end
  pop
  ```
  String stored in `_DATA`; FIXUPP at the imm16
  resolves to the literal's offset at link time.
  
  Callee uses `while (*s++)` pattern: save s in
  bx, inc s, cmp byte [bx], 0, jne loop.
- `2028` (**sizeof types**): all values resolved
  at parse time:
  ```c
  sizeof(int) = 2, sizeof(long) = 4
  sizeof(char) = 1, sizeof(int *) = 2 (small model)
  ```
  Each emits `c7 46 disp imm16` storing the
  constant — no runtime sizeof computation.
- `2029` (**`return (a, b)` yields b**): comma
  in return evaluates both subexpressions in
  order, returns the value of the LAST:
  ```
  ; x = x + 1 (side effect)
  mov ax, si / inc ax / mov si, ax
  ; y = y * 2 (side effect + returned value)
  mov ax, di / shl ax, 1 / mov di, ax
  ; ax holds the last result (= new y)
  ret
  ```
  Standard comma operator semantics.

For the Rust reimplementation:
- String literal args: emit string in `_DATA`,
  push FIXUPP'd offset at call site.
- sizeof: resolve at parse time using the type
  table.
- Comma in return: emit all subexpressions; the
  last one's AX is the return value.

## `if(x)` ≡ `if(x!=0)` ident codegen; `while(x--)` captures-then-decs; arg eval R-to-L confirmed

Fixtures `2024` (if x vs if x!=0), `2025` (while
x--), `2026` (fn call side-effect args) confirm
three patterns.

- `2024` (**`if (x)` ≡ `if (x != 0)`**): both
  forms produce **IDENTICAL bytes** — `or si, si
  / je skip`. BCC recognises the explicit `!= 0`
  comparison as equivalent to truthiness.
  
  This means programmers can write either form
  with no codegen difference; BCC normalises
  both to the zero-test idiom.
- `2025` (**`while (x--)` captures OLD value**):
  ```
  body:
    inc si              ; count++
  test:
    mov ax, di          ; capture OLD x
    dec di              ; x-- (post-dec)
    or ax, ax           ; test OLD value
    jne body            ; loop while OLD != 0
  ```
  Critical: the test uses the **pre-decrement
  value**. For x=5, loop runs 5 iterations
  (testing 5,4,3,2,1); on x=0 the test sees 0
  and exits (x then becomes -1).
- `2026` (**arg eval right-to-left, confirmed**):
  `add(trace(1), trace(2))` evaluates:
  ```
  push 2 / call trace / pop      ; trace(2) first
  push ax                         ; save trace(2) result
  push 1 / call trace / pop      ; trace(1) second
  push ax                         ; save trace(1) result
  call add                        ; add(t1, t2)
  ```
  Right-to-left: trace(2) before trace(1).
  Matches the cdecl push order. Side-effects
  observable as right-to-left.

For the Rust reimplementation:
- Normalise `if (x)` and `if (x != 0)` to the
  same test (or reg, reg / jcc).
- `while (x--)`: emit capture-then-decrement
  before the test.
- Fn arg evaluation: emit subexpressions
  right-to-left, with each result pushed before
  the next is evaluated.

## `if (0)` skip via jmp; `if (1)` fall-through no test; `while (0)` jmp past body — bodies still emitted

Fixtures `2021` (if 0), `2022` (if 1), `2023`
(while 0) show **constant-condition control-flow
folding**.

- `2021` (**`if (0) ... else ...`**): emits
  unconditional **`jmp <else>`** at the top —
  no cmp/jcc test. Then-body is dead code in
  output:
  ```
  jmp L_else
  L_then: mov si, 99   ; dead
  jmp L_end
  L_else: mov si, 5     ; executed
  L_end:
  ```
- `2022` (**`if (1) ... else ...`**): NO cmp/jcc
  test, NO unconditional jmp. Just **fall-through
  to then**; jmp over else:
  ```
  mov si, 5             ; then-body, executed
  jmp L_end             ; skip else
  L_else: mov si, 99    ; dead
  L_end:
  ```
- `2023` (**`while (0) body`**): emits **`jmp
  past-body`** at the top — no init jmp / cmp /
  jcc structure. Body is dead:
  ```
  jmp L_end
  L_body: mov si, 99    ; dead
  L_end:
  ```

So constant conditions are **recognised at parse
time**, eliminating the cmp/jcc test, but **both
branches are still emitted** as dead code (no
DCE).

**Constant-condition control-flow summary**:
| Pattern | Codegen |
|---------|---------|
| `if (literal-0) T else E` | `jmp E` + then(dead) + jmp + else |
| `if (literal-1) T else E` | (no test) + then + jmp + else(dead) |
| `if (literal-0) T` (no else) | (skip via jmp) + then(dead) |
| `if (literal-1) T` (no else) | (no test) + then |
| `while (literal-0) body` | `jmp past-body` + body(dead) |
| `while (literal-1) body` | body + `jmp top` (infinite loop, no test) |
| `do {body} while (0)` | body + (no jcc, fall through to end) |
| `do {body} while (1)` | body + `jmp top` (infinite) |

For the Rust reimplementation:
- Constant-cond if/while: detect literal-0 / literal-1
  at parse time; emit direct jmp/fall-through.
- Don't perform DCE; emit dead bodies as-is.

## Const-expr fully folded; adjacent consts combined (`x+5+3`→`x+8`); `x && 0` → direct false-jmp

Fixtures `2018` (full const expr), `2019` (`x + 5
+ 3` const-combination), `2020` (`x && 0`) show
that BCC's parse-time folding is **more
sophisticated** than just literal identity.

- `2018` (**full const expression**): `(2*3) +
  (4*5) - 1` is fully computed at parse time:
  ```
  mov word [r], 0x19    ; r = 25 (direct constant store)
  ```
  All operators evaluated; only the final
  constant emitted.
- `2019` (**adjacent constants combined**): `x +
  5 + 3` parsed as `((x + 5) + 3)` but BCC
  **combines the constants** at parse time:
  ```
  mov ax, [x]
  add ax, 8             ; not 5+3 separately, but the sum 8
  ```
  So adjacent-constant folding works **across
  left-to-right associative expressions**, not
  just `K op K` cases.
- `2020` (**`x && 0` → direct false-jmp**):
  ```
  cmp [x], 0
  je  L_false           ; first operand's inverse-jcc
  jmp L_false           ; second operand is literal-0 → unconditional jmp to false
  L_true: mov ax, 1     ; dead code (unreachable)
  jmp end
  L_false: xor ax, ax
  end:
  ```
  The second operand being literal 0 produces an
  **unconditional `jmp L_false`** (since 0 is
  always false). The "true" branch becomes dead
  code in the output but is still emitted.
  
  So BCC recognises **literal boolean constants**
  in && and || and emits direct jumps, but
  doesn't eliminate the resulting dead code.

**Updated folding catalog**:
| Pattern | Effect |
|---------|--------|
| All-const expr | Computed at parse time |
| `x op K1 op K2` (associative) | Constants combined first |
| `x && literal-0` | Direct jmp to false branch (body dead) |
| `x || literal-1` | Direct jmp to true branch (else dead) |
| `0 + x`, `x + 0`, `x - 0` | → `x` (identity) |
| `0 * x`, `x * 0` | → 0 (zero-product) |
| `x ^ x`, `x - x` | NOT folded (no var-identity) |
| `x & -1` | NOT folded (only literal 0/1) |

For the Rust reimplementation:
- Full const expression folding via recursive
  evaluation.
- Combine adjacent constants in associative
  chains.
- Boolean literal in && / ||: emit direct jmp
  past the dead branch.
- Don't bother with DCE — keep emitting dead
  code as it appears in the source.

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

## Confirmed: `x - 0`, `x ^ 0`, `x / 1` all identity-folded to bare `mov`

Fixtures `2012` (x - 0), `2013` (x ^ 0), `2014`
(x / 1) — all three produce **byte-identical OBJ
files** with the same `mov ax, x / mov [r], ax`
sequence. No sub/xor/idiv emitted.

So the identity-folding catalog is **fully
confirmed**:
| Operation | Result |
|-----------|--------|
| `x + 0`, `0 + x`, `x - 0` | → load x |
| `x | 0`, `0 | x`, `x ^ 0` | → load x |
| `x * 1`, `1 * x`, `x / 1` | → load x |
| `x * 0`, `0 * x` | → store 0 |

All emit **identical 8-byte bodies** (`8b 46 fe /
89 46 fc / 8b 46 fc / eb 00`, plus prologue/
epilogue). The arithmetic is completely
eliminated at parse time.

Notably, `x - 0` is NOT lowered to `add ax,
-0` or anything — it's truly folded. Same for
all the others.

This confirms BCC's **parse-time arithmetic
folding** is comprehensive for identity ops
across all major operator categories (add/sub,
and/or/xor, mul/div).

What WOULD NOT be folded (presumably):
- `x ^ x` → not folded (BCC doesn't track variable
  identity)
- `x - x` → not folded
- `x & 0xFFFF` → not folded (the mask isn't
  recognized as identity for 16-bit type)

For the Rust reimplementation:
- Implement identity folding for: + 0, - 0, * 1,
  / 1, | 0, ^ 0, & -1, * 0 (= zero).
- Do NOT attempt variable-identity simplification
  (e.g., x - x → 0).

## Identity folds: `x + 0` = mov; `x | 0` = mov; `x * 0` = direct store of 0

Fixtures `2009` (x + 0), `2010` (x | 0), `2011`
(x * 0) confirm/extend the identity-folding
catalog.

- `2009` (**`x + 0` identity-folded**): NO `add`
  emitted. Just `mov ax, x`. The +0 is recognised
  at parse time as a no-op.
- `2010` (**`x | 0` identity-folded**): NO `or`
  emitted. Just `mov ax, x`. The OR with 0
  preserves the value.
- `2011` (**`x * 0` zero-folded directly**): NO
  `imul` AND NO load of x. Emits `c7 46 disp 00
  00` (direct store of 0) — the entire
  computation is replaced by the constant 0.
  
  Notable: x is still **stored** (its assignment
  emits `c7 46 fe 2a 00`), even though never
  used. Confirms BCC's no-DCE policy — only the
  ARITHMETIC EXPRESSION is folded; the
  surrounding statements are emitted as-is.

**Complete identity/constant-folding catalog**:
| Operation | Fold |
|-----------|------|
| `x + 0`, `0 + x` | → `x` (just load) |
| `x - 0` | → `x` (probably) |
| `x | 0` | → `x` |
| `x ^ 0` | → `x` (probably) |
| `x & -1` (all ones) | → `x` (probably) |
| `x * 1`, `1 * x` | → `x` |
| `x * 0`, `0 * x` | → `0` (direct store) |
| `x / 1` | → `x` (probably) |
| Constant + constant | computed at parse time |
| Any expression of compile-time constants | computed at parse time |

So BCC's optimisation focus is **identity ops
and compile-time constants**, not data-flow or
algebraic simplifications. The folds happen at
parse time before codegen sees the expression.

For the Rust reimplementation:
- Identity ops: detect `K op X` and `X op K` for
  known identities; emit the simpler form.
- Zero-product: `X * 0` → direct constant 0 emit.
- Const-const: compute at parse time, emit
  result.

## Stack frames word-aligned (127→128); `x * 1` identity-folded away

Fixtures `2006` (127B frame), `2007` (128B frame),
`2008` (mul by 1) cover frame-alignment and
identity-folding.

- `2006` (**127B local array → 128B frame**):
  `char a[127]` allocates **128 bytes** on stack
  (word-aligned, rounded up). Sub-sp uses
  imm16 form since 128 > imm8-sext max:
  ```
  81 ec 80 00            ; sub sp, 128
  ```
  `a[0]` at `[bp-128]` (= 0x80 = imm8-sext for
  -128, fits disp8). `a[126]` at `[bp-2]` (disp8).
- `2007` (**128B local array → 128B frame**):
  same `81 ec 80 00`. `a[127]` at `[bp-1]`.
  Both arrays end up with the same 128-byte
  allocation.
- `2008` (**`x * 1` identity-folded**): `x * 1`
  is recognised at parse time as **identity**:
  ```
  mov ax, x             ; just load, no mul
  mov [r], ax
  ```
  No `imul` emitted. Adds to the optimization
  catalog:
  - `x * 1` → `mov` (just load)
  - `x * 0` → presumably also folded (not yet probed)
  - `x + 0`, `x - 0`, `x | 0`, `x & -1` → likely
    also folded
  - `x ^ x` → presumably NOT folded (BCC doesn't
    seem to track variable identity)

**Stack frame size rule**:
| Source local size | Allocated bytes |
|-------------------|------------------|
| Even N | N |
| Odd N | N + 1 (round up to word) |
| > 127 | uses `81 ec imm16` for sub sp |
| ≤ 127 | uses `83 ec imm8-sext` for sub sp |
| N = 1, 2 | `dec sp` × N (2-byte total) |

For the Rust reimplementation:
- Stack frame: word-align by rounding odd-byte
  totals up.
- Identity folding: `x * 1` → load; similar for
  other ops with identity-constants.

## Large local arr uses `sub sp imm16`+disp16 access; nested calls chain; char args = no auto-promotion

Fixtures `2003` (large local array — 200 bytes),
`2004` (deeply nested calls), `2005` (mixed
int/char args) cover three more shapes.

- `2003` (**large local array**): `int a[100]`
  needs **200 bytes**. Stack allocation uses
  imm16 form:
  ```
  81 ec c8 00            ; sub sp, 200 (imm16, since 200 > imm8-sext)
  ```
  Element access uses **per-element disp8/disp16
  choice**:
  - `a[0]` at `[bp-200]` (= bp+0xff38): needs
    disp16, uses `c7 86 disp16 imm16` (6 bytes)
  - `a[50]` at `[bp-100]` (= bp+0x9c): fits
    disp8 sign-extended (-100), uses `c7 46 disp8
    imm16` (5 bytes)
  - `a[99]` at `[bp-2]`: disp8, 5 bytes
  
  ModR/M variants:
  - `46 disp8` = mod=01 [bp+disp8]
  - `86 disp16 disp16` = mod=10 [bp+disp16]
- `2004` (**5-deep nested call**): each `s(...)`
  is push/call/pop/push for the next:
  ```
  xor ax, ax / push ax / call s / pop / push ax / call s / ... 
  ```
  5 calls = 30 bytes of call-overhead bytes
  (5 × 6 bytes each: 1 push + 3 call + 1 pop + 1
  push). Result flows through AX.
- `2005` (**mixed int/char args — no auto-promote
  when proto matches**): when calling
  `sum(int, char, int, char)` with a `char`
  param, the caller emits **byte mov + push word
  with garbage high byte**:
  ```
  mov al, 'B' / push ax        ; high byte = garbage
  ```
  Caller does NOT promote char→int when the
  callee's prototype says the param IS a char.
  Callee uses byte ops on the low half.
  
  Contrasts with [[1993-uchar-promotion]]: when
  passing `char` to a fn taking `int`, caller
  promotes (cbw/mov ah, 0). The promotion
  depends on the **callee's prototype**.

So **C's prototype matters** for arg passing:
- Param type matches arg type: pass as-is (with
  garbage high bytes for sub-int sizes)
- Param type is int, arg type is char: caller
  promotes (cbw for signed, mov ah, 0 for unsigned)

For the Rust reimplementation:
- Large frame allocation: emit `81 ec imm16` for
  >127 bytes.
- Per-element disp8/disp16 selection based on
  offset magnitude.
- Arg passing: consult callee's prototype to
  decide whether to promote sub-int args.

## `sum = *p++` (byte) = `mov al,[si]/cbw/inc si`; cmp reg,imm16 = `81 /7`; large arg = mov+push

Fixtures `2000` (byte read postinc), `2001` (cmp
with imm16), `2002` (large imm arg) cover three
remaining patterns.

- `2000` (**`sum = *p++` for byte ptr**): emits:
  ```
  mov al, [si]            ; 8a 04 — byte load (2B)
  cbw                      ; 98 — sign-ext (1B, since char signed)
  ; ... (use AX)
  inc si                   ; 46 — p++ (1B)
  ```
  Total 4 bytes per read+increment. For pure
  `*p` without post-inc, omit the `inc si`.
- `2001` (**cmp r16, imm16 not fitting imm8-sext**):
  emits **`81 /7 reg imm16`** (4 bytes):
  ```
  81 fe f4 01            ; cmp si, 500
  ```
  ModR/M `fe` = mod=11 reg=111 (/7) rm=110 (SI).
  Compare to imm8-sext form (`83 /7 reg imm8`, 3
  bytes) for values fitting -128..127.
- `2002` (**large immediate arg**): `identity(12345)`
  emits standard `mov ax, imm16 / push ax`:
  ```
  b8 39 30                ; mov ax, 12345
  50                       ; push ax
  ```
  4 bytes total. Same pattern regardless of value
  magnitude. No 80186+ `push imm16` shortcut.

**Byte vs word arithmetic-encoding hierarchy**:
| Operation | Byte (1B operand) | Word (2B operand) |
|-----------|--------------------|--------------------|
| Load const | `mov al, imm8` (2B) | `mov ax, imm16` (3B) |
| Load [m] | `mov al, [m]` (3B, AX-form) | `mov ax, [m]` (3B, AX-form) |
| Store imm | `mov byte [m], imm8` (4B, `c6 /0`) | `mov word [m], imm16` (5B, `c7 /0`) |
| Cmp imm fit imm8 | `cmp byte [m], imm8` (4B, `80 /7`) | `cmp word [m], imm8-sext` (4B, `83 /7`) |
| Cmp imm16 | (n/a) | `cmp word [m], imm16` (6B, `81 /7`) |
| Inc/dec ptr | `inc reg` (1B) | `inc reg / inc reg` (2B for int*) |

For the Rust reimplementation:
- Byte read+postinc: emit `mov al, [reg] / cbw /
  inc reg` (for char) or omit cbw (for uchar +
  emit `mov ah, 0` if int needed).
- Cmp r16 imm16 (non-imm8-sext): use `81 /7
  imm16`.
- Large imm args: `mov ax / push ax`, no
  80186-only shortcuts.

## Narrowing casts = direct low-byte/word access; `*p++ = byte` = `mov [si],imm8 / inc si`

Fixtures `1997` (int→char), `1998` (long→int),
`1999` (byte writes via post-inc ptr) cover
narrowing casts and byte-write idioms.

- `1997` (**`(char)int` narrowing**): direct low-
  byte read — `mov al, [x]`. Since int is stored
  little-endian, the low byte is at the variable's
  base address. No mask/shift needed.
  ```
  mov al, [x]              ; load low byte (low addr)
  mov [c], al              ; byte store
  ```
- `1998` (**`(int)long` narrowing**): direct low-
  word read — `mov ax, [x.lo]` (the low half is
  at the lower offset).
  ```
  mov ax, [x.lo]           ; mov ax, [bp-4] (low at lower addr)
  mov [n], ax
  ```
  No truncation instruction; the type system gives
  byte-precise access to the wanted part.
- `1999` (**`*p++ = byte_const`**): emits **`mov
  byte [si], imm8 / inc si`** (4 bytes):
  ```
  c6 04 'A'                ; mov byte [si], 'A'
  46                        ; inc si (post-inc)
  ```
  ModR/M `04` = mod=00 reg=000 rm=100 = [SI].
  Store **before** increment, matching post-inc
  semantics with the assignment expression's
  value.

**Narrowing-cast summary**:
| Cast | Mechanism |
|------|-----------|
| `(char)int` | Read low byte at the variable's base addr |
| `(int)long` | Read low word at the variable's base addr |
| `(char)long` | Read byte at base (= low byte of low half) |
| `(int)expression-in-AX` | No-op (AX is already a word) |
| `(char)expression-in-AX` | Use AL (low byte), AH undefined |

For the Rust reimplementation:
- Narrowing casts: emit direct partial-read at
  the lower-offset bytes; no mask/shift needed.
- Byte writes via post-inc ptr: emit `mov byte
  [reg], imm8 / inc reg` per source statement.

## `short` == `int` on 8086; multi static locals = declaration order in `_DATA`; fn-ptr arr call

Fixtures `1994` (short vs int), `1995` (multi
static locals), `1996` (fn-ptr array call) cover
type aliasing and storage layout.

- `1994` (**`short` is `int`**): in BCC 2.0,
  `short` and `int` are **both 2 bytes**. Stored
  with `c7 46 disp imm16` for both — no
  distinction. The `short` keyword is purely
  source-level; codegen treats them identically.
  
  C type sizes in BCC 2.0:
  | Type | Size |
  |------|------|
  | char, unsigned char | 1 byte |
  | int, unsigned int, short, unsigned short | 2 bytes |
  | long, unsigned long | 4 bytes |
  | float | 4 bytes |
  | double | 8 bytes |
  | near ptr | 2 bytes |
  | far ptr | 4 bytes |
- `1995` (**multi static locals = declaration
  order**): `static int id; static int count;`
  in one function lays them out in `_DATA` in
  **source-declaration order**:
  - id at offset 0
  - count at offset 2
  
  Each accessed via `[disp16]` direct addressing
  with FIXUPP to the function-local static block.
  Zero-init statics get 4 bytes total in `_DATA`
  (could also live in `_BSS` for the zero-init
  case, but here BCC puts them in `_DATA`).
- `1996` (**fn-ptr array call**): same pattern as
  fixture [[1918-array-of-fn-ptrs]]:
  ```
  c7 46 fc 00 00       ; ops[0] = _op_add (FIXUPP)
  c7 46 fe 0d 00       ; ops[1] = _op_sub
  ff 56 fc             ; call near [ops[0]]
  ff 56 fe             ; call near [ops[1]]
  ```
  Each call indirect via `[bp+disp]` slot using
  `ff /2` opcode.

For the Rust reimplementation:
- Type tracking: short, int, unsigned short,
  unsigned int → 2-byte. Treat as same kind for
  codegen purposes.
- Static locals: emit each at successive offsets
  in `_DATA` for the containing function.

## uchar arg passed as word (hi undef); 2D char arr flat row-major; caller promotes byte args

Fixtures `1991` (uchar arg), `1992` (2D char
array init), `1993` (uchar args promoted) cover
char/uchar parameter passing semantics.

- `1991` (**uchar arg passed as full word**):
  even though the param is `unsigned char`, BCC
  pushes a **16-bit word** (high byte
  undefined). Callee uses byte ops on the low
  half:
  ```
  ; in callee:
  mov bl, [bp+4]            ; load byte from arg slot
  ; ... operate on bl, zero-extend via b4 00 as needed
  ```
  Caller pushes via `mov al, [c] / push ax` —
  high byte AH whatever-was-there.
- `1992` (**2D char array init**): `char grid[2]
  [3]` lays out **flat 6 bytes row-major** in
  `_DATA` template ("ABCDEF"); N_SCOPY@ copies
  to stack at fn entry. Constant indices resolve
  to byte offsets via row*width + col.
- `1993` (**uchar args promoted to int at call
  site**): when passing `unsigned char` values
  to a fn taking `int`:
  ```
  mov al, [x]               ; load byte
  mov ah, 0                 ; zero-extend
  push ax                   ; push as int
  ```
  Caller does the byte→int promotion **before
  the push**. Consistent with C's integer-
  promotion rules.

**Char/uchar promotion summary**:
| Source | Target | Where | Mechanism |
|--------|--------|-------|-----------|
| `char` value | `int` use | Site of use | `cbw` (1B, sign-ext) |
| `unsigned char` value | `int` use | Site of use | `mov ah, 0` (2B, zero-ext) |
| `char` arg | `int` arg | At call site | `cbw` then push |
| `unsigned char` arg | `int` arg | At call site | `mov ah, 0` then push |
| Byte return | Byte | In AL only | High half undef |

So **callers handle the promotion**, not callees.
This matches K&R C semantics where all char args
are promoted to int by the caller.

For the Rust reimplementation:
- Track char/uchar/short types through expressions.
- Emit sign/zero-extend at promotion points.
- Caller emits the promotion before push; callee
  reads only the low half (signed extension is
  caller's responsibility for sub-int args).

## uchar→int via `mov ah, 0`; bool arith materialises each cmp; empty-body while w/ side-effect-cond

Fixtures `1988` (unsigned char return), `1989`
(bool arith), `1990` (empty-body while) cover
three more idioms.

- `1988` (**unsigned char → int = `mov ah, 0`**):
  unsigned char return in AL only. Caller zero-
  extends with `mov ah, 0` (`b4 00`, 2 bytes):
  ```
  call _get_ub
  mov [c], al               ; byte store
  mov al, [c] / mov ah, 0   ; load + zero-extend
  ```
  Compare to signed char which uses `cbw` (1 byte
  sign-extend).
  
  **Char → int conversion summary**:
  | Source type | Extension | Bytes |
  |-------------|-----------|-------|
  | `char` (signed) | `cbw` | 1 |
  | `unsigned char` | `mov ah, 0` | 2 |
- `1989` (**bool arith — each cmp materialised**):
  `(a == b) + (a == c)` materialises **each
  comparison separately** via the full bool
  template, then sums:
  ```
  ; first cmp:
  cmp si, [b] / jne L_f1 / mov ax, 1 / jmp end1
  L_f1: xor ax, ax
  end1: push ax              ; save 1st bool
  ; second cmp:
  cmp si, [c] / jne L_f2 / mov ax, 1 / jmp end2
  L_f2: xor ax, ax
  end2: pop dx / add dx, ax  ; combine
  ```
  No fusion; each comparison generates a full
  template. Booleans treated as ints (0 or 1).
- `1990` (**empty-body while with side-effect**):
  `while (fn() < 5) ;` confirms the pattern:
  ```
  jmp test
  body:        ; empty
  test:
    call _inc_counter
    cmp ax, 5
    jl body
  ```
  No body instructions; just the test repeats
  until false. The fn call's side effect (++counter)
  is the only loop progress.

For the Rust reimplementation:
- Unsigned char → int: emit `mov ah, 0`; signed
  char → int: emit `cbw`.
- Bool arith in expressions: materialise each
  cmp via the value-context template.
- Empty-body loops: still emit `jmp test / body /
  test` skeleton.

## Unsigned `<=` uses `ja`; bounds check via short-circuit; `char` ret = AL only + cbw

Fixtures `1985` (unsigned `<=`), `1986` (bounds
check pattern), `1987` (char return) close out
common idioms.

- `1985` (**unsigned `a <= b`**): false-branch jcc
  is **`ja`** (`0x77`, unsigned above) — inverse
  of `<=`. Completes the unsigned-cmp jcc table.
- `1986` (**bounds check `i >= 0 && i < 5`**):
  short-circuit && with signed-cmp per operand:
  ```
  ; i >= 0 test:
  or si, si              ; cheap zero-test for i
  jl L_else              ; if i < 0, branch out
  ; i < 5 test:
  cmp si, 5
  jge L_else             ; if i >= 5, branch out
  ; ... bounds-check passed body
  ```
  Each operand's inverse-jcc goes to the same
  L_else target. `i >= 0` uses `or si, si` for
  the cheaper zero-test (since the constant is
  0).
- `1987` (**`char` return = AL only**): function
  returning `char` sets **only AL** (low byte of
  AX); AH is undefined. Caller:
  ```
  call _get_char
  mov [c], al              ; 88 46 ff — byte store
  mov al, [c]              ; 8a 46 ff — byte load
  cbw                      ; 98 — sign-extend to int (since char is signed)
  ```
  Char locals get **byte-sized stack slots** at
  odd offsets (e.g., `[bp-1]`).

**Unsigned-cmp jcc table** (complete, for false-
branch):
| Op | Unsigned false-jcc | Opcode |
|----|--------------------|--------|
| `<`  | `jae` (`jnc`)    | 73 |
| `<=` | `ja`             | 77 |
| `>`  | `jbe`            | 76 |
| `>=` | `jb` (`jc`)      | 72 |
| `==` | `jne`            | 75 |
| `!=` | `je`             | 74 |

For the Rust reimplementation:
- Unsigned-cmp jcc choice: use ja/jbe/jae/jb
  per operator (false-branch is inverse).
- char return: emit `mov al, val` only; caller
  treats AL as the byte result.

## Unsigned cmp uses `jbe`/`jae` for inverse; ptr arith scales by sizeof; ptr sub = `idiv`

Fixtures `1982` (unsigned `x > 100`), `1983`
(char* vs int* arithmetic), `1984` (`q - p` ptr
subtract) cover pointer arithmetic semantics.

- `1982` (**unsigned cmp uses unsigned jcc**):
  `unsigned x > 100` emits:
  ```
  cmp word [x], 100      ; 83 7e disp 64 (imm8-sext)
  jbe L_false             ; 76 — unsigned below-or-equal
  ```
  Uses `jbe` (`0x76`) as the false-branch jcc.
  Operands' unsigned type → unsigned jcc, even
  for small constants.
- `1983` (**ptr arith scales by sizeof at parse**):
  ```c
  char *cp; cp += 1;    // inc cp (+1 byte)
  int *ip;  ip += 1;    // inc ip; inc ip (+2 bytes)
  ```
  emits:
  ```
  46            ; inc si (cp by sizeof(char) = 1)
  47 47         ; inc di; inc di (ip by sizeof(int) = 2)
  ```
  The `+= 1` is **silently multiplied by
  sizeof(element)** at parse time. For pow2 sizes
  (1, 2, 4) the increment is direct; for odd
  sizes (e.g., 3 for a 3-byte struct) would use
  `add ptr, K`.
- `1984` (**ptr subtraction divides by sizeof**):
  `q - p` for `int *` pointers emits:
  ```
  mov ax, [q]
  sub ax, [p]            ; byte difference
  mov bx, 2              ; sizeof(int)
  cwd
  idiv bx                ; signed divide → element count
  ```
  Result is the **element count**, signed (can be
  negative if q < p). Uses `idiv` (not shr) for
  general correctness across signs.
  
  For sizeof = 1 (char*), no division needed.
  For other sizes, divide by sizeof.

For the Rust reimplementation:
- Track operand signedness for all cmps; use
  jbe/jae/ja/jb for unsigned, jle/jge/jg/jl for
  signed.
- Pointer arithmetic: scale increments/decrements
  by sizeof(element) at parse time. Emit minimal
  inc count for pow2; add for non-pow2.
- Pointer subtraction: emit byte-diff then
  signed-divide by sizeof.

## Pool fill order confirmed `{SI, BX, DI, CX, DX}`; 1 fn call enough to restrict; mul+call combo

Fixtures `1979` (5 locals, no mul/call), `1980`
(5 locals + 1 fn call), `1981` (mul + call mix)
pin down the register-allocation context rules.

- `1979` (**pool fill order = {SI, BX, DI, CX,
  DX}**): with no call/mul, 5 locals enregister
  in order:
  - a → SI
  - b → BX
  - c → DI
  - d → CX
  - e → DX
  
  So the **pool fill order** is:
  ```
  {SI, BX, DI, CX, DX}
  ```
  Declaration-order assignment, this is the
  preference sequence.
- `1980` (**1 fn call → already restricted**):
  with only ONE fn call in the function:
  - a → SI
  - c → DI (skipping b, since b would be in BX
    which is caller-saved)
  - b, d, e → stack
  
  The restriction is **presence-based**, not
  frequency-based. Any fn call disables the
  BX/CX/DX slots for enregistration.
- `1981` (**mul + call combo**): both restrictions
  apply. Pool = {SI, DI}, but in this fixture
  the locals are each used only once (below
  the ≥2-read threshold), so nothing enregisters
  at all.

**Final register-allocation rule** (definitive):
1. **Identify candidates**: locals/params with
   ≥2 reads in expression contexts.
2. **Determine pool**:
   - With fn calls → pool = {SI, DI} (2 slots)
   - With imul/idiv but no fn calls → pool = {SI,
     BX, DI, CX} (4 slots)
   - Else → pool = {SI, BX, DI, CX, DX} (5 slots)
3. **Assign candidates** in declaration order to
   pool slots in pool-fill order.
4. **Overflow candidates** go to stack.

For the Rust reimplementation: implement this
exact 4-step allocator. The pool fill order
must be SI, BX, DI, CX, DX (preserving SI as
first since it's callee-saved and often used
for the primary local/accumulator).

## 5 regs w/o mul; **fn calls restrict pool to callee-saved {SI, DI}**; empty fn keeps prologue

Fixtures `1976` (7 locals, NO mul), `1977` (fn
call restricts pool), `1978` (empty fn keeps
prologue) clarify register-allocation context-
sensitivity.

- `1976` (**7 locals, no mul → DX used**):
  without imul/idiv, BCC enregisters **5 vars**:
  - a → DI
  - b → DX
  - c → BX
  - d → CX
  - r (sum) → SI
  - e, f, g → stack
  
  All 5 pool registers used. Confirms: without
  mul/div, the full 5-register pool {SI, DI, BX,
  CX, DX} is available.
- `1977` (**fn calls restrict pool to {SI, DI}**):
  this is a **major refinement**. With fn calls
  present, only **2 registers** (SI and DI)
  enregister:
  - a → DI
  - r → SI
  - b, c → stack
  
  Because cdecl callee-saved registers are SI/DI,
  but BX/CX/DX are **caller-saved** (callee can
  clobber them). BCC's register allocator
  detects fn calls and restricts to **callee-
  saved-only** to avoid the need for save/restore
  around every call.
- `1978` (**empty fn keeps prologue**): confirms
  once more — `int empty(void) { return 0; }`
  emits full `push bp / mov bp, sp / xor ax,ax /
  pop bp / ret`. No bp-omission optimization
  regardless of frame need.

**Register-allocation context table (revised)**:
| Function characteristics | Available pool | Notes |
|--------------------------|----------------|-------|
| No mul/div, no fn calls | {SI, DI, BX, CX, DX} = 5 slots | Full pool |
| With imul/idiv, no fn calls | {SI, DI, BX, CX} = 4 slots | DX reserved as imul high |
| With fn calls | {SI, DI} = 2 slots | Callee-saved only |
| With both | {SI, DI} = 2 slots | Most restrictive |

For the Rust reimplementation:
- Analyze function body for: fn calls, mul/div
  ops.
- Choose pool accordingly:
  - Fn calls present → restrict to {SI, DI}
  - imul/idiv present → exclude DX
  - Else → full pool
- Locals ranked by use-count (or declaration
  order); assign in pool order.

This explains why functions with many fn calls
often have most locals on the stack — BCC can't
safely use the AX/BX/CX/DX registers across the
calls.

## 7 locals: only 4 enregister (DX reserved); nested calls use arg-stack; mixed cmp via cast

Fixtures `1973` (7 multi-use locals), `1974`
(`f(g(...), h(...))` nested), `1975` (mixed
signed/unsigned cmp) cover more register-
allocator behavior.

- `1973` (**7 locals → only 4 enregister**):
  with 8 multi-read candidates (7 locals + 1
  derived `r`), BCC enregisters **only 4**:
  - `a` → DI
  - `b` → BX
  - `c` → CX
  - `r` → SI (the accumulator)
  - `d, e, f, g` → stack
  
  So **DX is NOT used** for a local. Likely
  reserved as scratch (especially because the
  function uses `imul`, which produces the high
  half in DX). With imul present, the pool
  effectively becomes 4: {SI, DI, BX, CX}.
  
  Refined rule:
  - Without imul/idiv: pool = {SI, DI, BX, CX, DX}
    (5 slots, see [[batch-511-five-locals]])
  - With imul/idiv: DX reserved, pool = {SI, DI,
    BX, CX} (4 slots)
- `1974` (**nested calls use arg-stack as scratch**):
  `f(g(...), h(...))` evaluates right-to-left:
  ```
  ; compute h(3, 4):
  push 4 / push 3 / call h / pop / pop          ; ax = 7
  push ax                                        ; save as outer's 2nd arg
  ; compute g(1, 2):
  push 2 / push 1 / call g / pop / pop          ; ax = 3
  push ax                                        ; save as outer's 1st arg
  ; call f:
  call f / pop / pop                             ; ax = result
  ```
  Each inner call's result is **pushed directly
  as the corresponding arg of the outer call**.
  No temporary stack variables; the args stack
  doubles as scratch.
- `1975` (**mixed signed/unsigned cmp via cast**):
  `(int)u` makes the comparison **signed**:
  ```
  mov ax, [s]                ; -1 (= 0xffff)
  cmp ax, [u]
  jge L_false                ; signed jge for inverse of <
  ```
  For s = -1, u = 1: signed `-1 < 1` is TRUE
  (return 1). Unsigned `0xffff < 1` would be
  FALSE. The cast forces signed-cmp semantics.
  
  Type-driven jcc choice: BCC tracks the type of
  each operand and chooses the appropriate jcc.

**Refined register-allocation pool**:
- No mul/div: {SI, DI, BX, CX, DX} = 5 slots
- With imul/idiv: {SI, DI, BX, CX} = 4 slots
  (DX reserved as imul's high-half target)

For the Rust reimplementation:
- Track whether the function uses mul/div ops;
  reserve DX accordingly.
- Nested calls: use args-stack as scratch for
  inner results.
- Cast-driven cmp signedness: track operand
  types through casts.

## Loop-body local not enregistered; arr/struct-arr full init uses N_SCOPY@ template

Fixtures `1970` (loop body local), `1971` (int
array full init), `1972` (array of struct init)
cover remaining init shapes.

- `1970` (**loop-body local NOT enregistered**):
  `int t = i * 2;` inside the for-body is
  allocated at `[bp-2]` and stored each
  iteration — NOT enregistered:
  ```
  body:
    mov ax, si / shl ax, 1     ; ax = i*2
    mov [t], ax                ; store to [bp-2]
    add di, [t]                ; sum += t
    inc si
  ```
  Even though `t` is used twice per iteration
  (init and add), BCC doesn't enregister it.
  Conservative: register allocator only
  considers function-scope locals, not block-
  scoped variables inside loops.
- `1971` (**full array init uses N_SCOPY@**):
  `int a[5] = {1,2,3,4,5}` uses the **same
  N_SCOPY@ protocol** as partial init:
  - `_DATA` holds the template (10 bytes of int
    values)
  - Stack array allocated via `sub sp, 10`
  - N_SCOPY@ copies template → stack at fn entry
  No alternative for fully-initialized arrays;
  always copy.
- `1972` (**array of struct init**): same
  protocol with the struct values laid out
  **flat** in `_DATA`:
  ```
  data: 01 00 02 00 03 00 04 00 05 00 06 00
        ^      ^      ^      ^      ^      ^
        arr[0].x .y   arr[1].x .y   arr[2].x .y
  ```
  12-byte copy via N_SCOPY@. Nested aggregates
  are flattened into a single linear template.

So **all array/struct initializers** use the
universal pattern:
1. Lay out the initialized data flat in `_DATA`
2. Allocate the stack space in fn prologue
3. Call N_SCOPY@ to copy template → stack at
   fn entry

For the Rust reimplementation:
- Loop-body locals: treat as block-scoped, no
  enregistration consideration.
- Aggregate initializers (array, struct, array
  of struct, struct of array, etc.): flatten
  into `_DATA` template, emit N_SCOPY@ in
  prologue.

## Block-slot reuse: N sequential blocks share; nested respects scope; byte-granular sharing

Fixtures `1967` (3 sequential blocks), `1968`
(nested blocks), `1969` (different-sized block
locals) refine the block-slot-reuse rule.

- `1967` (**N sequential blocks share slot**):
  three sequential blocks each with a local
  variable all share **the same `[bp-2]` slot**:
  ```
  ; block 1: a
  c7 46 fe 01 00   ; a = 1 at [bp-2]
  03 76 fe          ; sum += a
  ; block 2: b — reuses [bp-2]
  c7 46 fe 02 00   ; b = 2
  03 76 fe          ; sum += b
  ; block 3: c — reuses [bp-2] again
  c7 46 fe 03 00   ; c = 3
  03 76 fe          ; sum += c
  ```
  Only 1 slot allocated for N sequential
  non-overlapping locals.
- `1968` (**nested blocks respect outer scope**):
  ```c
  int outer = 100;
  { int inner = 50; outer += inner; }
  ```
  Outer enregisters (used twice — init + return)
  in SI. Inner gets `[bp-2]`. **No slot reuse
  between outer and inner** since their lifetimes
  overlap (outer is still alive when inner is
  created).
- `1969` (**byte-granular slot sharing**):
  different-sized block locals can share too:
  ```c
  { long a = 100L; sum += (int)a; }    // a = 4 bytes at [bp-4..bp-1]
  { int b = 50;    sum += b; }         // b = 2 bytes at [bp-2..bp-1]
  ```
  `b` reuses **the HIGH HALF of a's slot** (the
  top 2 bytes). So slot reuse is **byte-
  granular** — BCC tracks the actual byte ranges
  of dead variables, not just whole-variable
  slots.

So the block-slot-reuse algorithm is:
1. Track byte-range liveness for every local
2. When entering a block, look for **N
   contiguous dead bytes** (from finished
   scopes) before allocating new stack
3. Allocate at the lowest available range that
   fits

For the Rust reimplementation:
- Maintain a per-byte-range liveness map within
  the function's stack frame.
- On entering a block, scan for free byte-ranges
  of the required size; reuse if found.
- Else allocate new bytes at the bottom of the
  current frame (incrementing `sub sp` by the
  new variable's size).
- On block exit, mark the variable's byte range
  as dead/free.

This single optimization keeps stack frames as
compact as possible — frame size = max
"concurrently live local bytes" across the
function.

## `**pp` = 2 chained loads; post-inc captures-then-increments; **block-locals share slots**

Fixtures `1964` (`**pp` double deref), `1965`
(post-inc with result used), `1966` (block-scoped
locals) cover three patterns.

- `1964` (**`**pp` double deref**):
  ```
  mov si, [pp]        ; pp into si
  mov bx, [si]        ; *pp into bx
  mov ax, [bx]        ; **pp into ax
  ```
  Same chained-load pattern as struct deref or
  linked-list traversal. Two register-load steps;
  no special optimization.
- `1965` (**post-inc with result used**):
  `y = x++ + 10`:
  ```
  mov ax, si          ; capture OLD x (= 5)
  inc si              ; x becomes 6 (post-inc)
  add ax, 10          ; ax = 15
  mov [y], ax
  ```
  Standard pattern: capture pre-increment value
  before modifying. Confirmed across many fixtures.
- `1966` (**block-locals share stack slots!**):
  ```c
  int sum = 0;
  { int x = 10; sum += x; }
  { int y = 20; sum += y; }
  ```
  emits with **x and y SHARING `[bp-2]`** — since
  their scopes don't overlap, BCC reuses the
  stack slot:
  ```
  ; block 1:
  c7 46 fe 0a 00       ; x = 10 (at [bp-2])
  03 76 fe              ; sum += x
  ; block 2:
  c7 46 fe 14 00       ; y = 20 (at [bp-2] — SAME slot!)
  03 76 fe              ; sum += y
  ```
  Stack frame allocates only **1 slot (2 bytes)**
  for these two non-overlapping locals.
  
  This is a real **slot-reuse optimization** —
  BCC does perform some scope-based stack
  packing. Adds a meaningful exception to the
  "no optimizations" rule.

For the Rust reimplementation:
- Double deref `**p`: emit `mov si/bx, [p] / mov
  bx, [si] / mov ax, [bx]`.
- Post-inc capture-then-increment is universal
  across all use contexts.
- **Block-scoped locals**: track lexical scope;
  reuse stack slots for variables whose scopes
  don't overlap. Each fn computes max
  "concurrent live" locals to size its frame.

So the optimization catalog for BCC 2.0:
1. **Constant folding** for compile-time-known
   expressions (arithmetic, sizeof).
2. **Pow2 mul/div** → shift instructions.
3. **Unsigned mod-pow2** → AND-with-(N-1).
4. **`x ± 1`** → inc/dec.
5. **AX-form opcodes** when destination is AX.
6. **imm8-sext** for ADD/SUB/CMP when value fits.
7. **Inverse-jcc folding** for `!cmp` and bool
   contexts.
8. **Short-circuit `&&`/`||`** via jcc chains.
9. **Switch jump table** for ≥4 contiguous cases.
10. **`(int)(y >> 16)`** → direct read of y.hi.
11. **String literal concatenation** at parse.
12. **Sizeof DCE** for arrays only used in sizeof.
13. **Block-scoped local slot reuse** ← NEW.

Everything else is "compile each statement
independently with no fusion".

## Globals declared-order in `_DATA`; uninit globals in `_BSS`; static array persists across calls

Fixtures `1961` (multiple inited globals), `1962`
(mixed init/uninit globals), `1963` (static array
persists) cover global storage semantics.

- `1961` (**globals in `_DATA` declaration order**):
  ```c
  int a = 10; int b = 20; int c = 30;
  ```
  emits in `_DATA` as **`0a 00 14 00 1e 00`** (10,
  20, 30) at consecutive offsets 0, 2, 4. Each
  exported in PUBDEF separately. Access via
  direct addressing:
  - `a1 disp16` (mov ax, [m]) for the first AX
    load
  - `03 06 disp16` (add ax, [m]) for subsequent
    adds
  All disp16 values are FIXUPP'd at link time.
- `1962` (**init globals in `_DATA`, zero-init in
  `_BSS`**):
  ```c
  int initialized = 42;       // _DATA
  int zeroed;                  // _BSS
  int more_init = 99;          // _DATA
  ```
  OBJ has **two segments**:
  - `_DATA`: holds `initialized` and `more_init`
    with their explicit values
  - `_BSS`: holds `zeroed` (zero-filled at load
    time by DOS/runtime)
  
  This is the classic separation that keeps the
  EXE smaller (BSS doesn't store zeros explicitly).
- `1963` (**static array persists**): `static int
  data[3] = {7, 11, 13};` inside a function:
  - Stored in **`_DATA`** with initial values
  - Persists across function calls — not stack-
    allocated
  - Access via `[bx + table_addr]` with FIXUPP
  ```
  mov si, [i]
  mov bx, si / shl bx, 1            ; bx = i*2
  inc word [bx + table_offset]      ; increment
  mov bx, si / shl bx, 1            ; recompute
  mov ax, [bx + table_offset]       ; load
  ```
  Each call sees the previous call's
  modifications. Confirms `static` storage
  duration semantics — same as global, just
  with file-local linkage.

**Global storage segments summary**:
| Variable kind | Segment | Notes |
|---------------|---------|-------|
| `int x = K;` | `_DATA` | Initialized with value K |
| `int x;` | `_BSS` | Zero-filled at startup |
| `static int x = K;` (file scope) | `_DATA` | Internal linkage |
| `static int x;` (file scope) | `_BSS` | Zero, internal linkage |
| `static int x = K;` (in fn) | `_DATA` | Internal, persists |
| `static int x;` (in fn) | `_BSS` | Zero, persists |

For the Rust reimplementation:
- Track all globals; place initialized in `_DATA`
  with their values in declaration order, zero-
  inits in `_BSS`.
- Static locals: same as globals but with name
  mangling for file-local uniqueness.

## K&R `()` = `(void)`; `extern int` in EXTDEF + FIXUPP; expr-stmt computes but discards

Fixtures `1958` (K&R `()` fn decl), `1959`
(`extern int`), `1960` (discarded expr-stmt)
cover three smaller language features.

- `1958` (**K&R `()` = `(void)`**): `int get42()
  { return 42; }` with no `void` keyword
  compiles identically to `int get42(void)`.
  No proto means no arg-type checking — but
  for zero-arg fn definitions the codegen is
  the same. Call site emits no pushes, no
  cleanup.
- `1959` (**`extern int outer`**): the extern
  decl adds **`_outer` to EXTDEF** (external
  symbol table). Access uses **`a1 0000`** (mov
  ax direct address, with FIXUPP) — link-time
  resolves the imm16 to outer's actual offset.
  
  No storage allocated in this TU; another TU
  must define `outer`. The OBJ requires linking
  with a definition.
- `1960` (**discarded expr-stmt**): `x + 1;`
  (statement with unused result) **emits the
  computation**:
  ```
  mov ax, si       ; load x
  inc ax           ; compute x + 1
  ; (result in AX, discarded — no store)
  ```
  Wasteful but consistent with BCC's "compile
  each statement" rule. The increment is
  computed but not stored.
  
  Subsequent `x++` properly increments via `inc
  si` (the variable's register slot).

So **no dead-code elimination** even for
side-effect-free expressions. Every C source
expression generates instructions, regardless
of usefulness.

For the Rust reimplementation:
- K&R `()` fn defn: treat as `(void)` (no args)
  for codegen purposes.
- `extern` decls: add to EXTDEF, generate
  FIXUPPs at use sites.
- Expression statements: emit the computation
  even if result is discarded. No DCE.

## Struct modify via ptr; 2B struct returns in AX only; rotate idiom NOT recognised

Fixtures `1955` (struct modify via ptr), `1956`
(2B struct return), `1957` (rotate via shifts +
or) cover three smaller idioms.

- `1955` (**struct modify via ptr arg**): callee
  pattern:
  ```
  mov si, [p]              ; load ptr
  mov ax, [x] / mov [si], ax        ; p->x = x at offset 0
  mov ax, [y] / mov [si+2], ax      ; p->y = y at offset 2
  ```
  Each field uses `[si]` (2B) for offset 0, or
  `[si+disp]` (3B) for non-zero offsets. Ptr
  loaded once into SI for all field accesses.
- `1956` (**2B struct return = AX only**): a
  struct with a single 2-byte field returns in
  **just AX** (no DX). Same protocol as int:
  ```
  mov ax, [o.x]
  ret
  ```
  Caller does `mov [s.x], ax`. So **1-int
  struct return = int return**.
- `1957` (**rotate idiom NOT recognised**): `(x
  << 4) | (x >> 12)` (a logical rotate-left by 4)
  emits the literal sequence:
  ```
  mov ax, [x] / mov cl, 4 / shl ax, cl
  mov dx, [x] / mov cl, 12 / shr dx, cl
  or ax, dx
  ```
  ~10 bytes. BCC does **not recognize the rotate
  pattern** to emit `mov cl, 4 / rol ax, cl` (4
  bytes). No advanced pattern-matching beyond
  arithmetic constant folding.

**Struct return size matrix** (refined):
| Struct size | Return mechanism |
|-------------|------------------|
| 2 bytes (1 int) | AX only (same as int) |
| 4 bytes (2 ints) | DX:AX |
| > 4 bytes | Hidden dest ptr + N_SCOPY@ × 2 |

For the Rust reimplementation:
- Struct modify via ptr: load ptr to BX/SI, each
  field stored via `[reg+offset]`.
- Tiny structs (≤2B): return like the underlying
  scalar.
- No rotate idiom recognition — emit literal shifts
  + or.

## ≤4B struct asg = inline mov-pair; >4B = N_SCOPY@; many calls = push/pop accumulate

Fixtures `1952` (4B struct asg), `1953` (8B struct
asg), `1954` (many fn calls) cover struct-copy
and accumulation patterns.

- `1952` (**4B struct asg = inline mov-pair**):
  for `struct P {int x; int y;}` (4 bytes), `b =
  a` emits:
  ```
  mov ax, [a.y]       ; load high field
  mov dx, [a.x]       ; load low field
  mov [b.y], ax
  mov [b.x], dx
  ```
  All fields loaded into registers (AX + DX),
  then stored. No helper call. Fast for small
  structs.
- `1953` (**>4B struct asg uses N_SCOPY@**): same
  threshold as pass-by-value. For 8-byte struct:
  ```
  lea ax, [y]            ; dest offset
  push ss / push ax      ; dest far ptr
  lea ax, [x]            ; source offset
  push ss / push ax      ; source far ptr
  mov cx, 8              ; size
  call N_SCOPY@
  ```
  Same protocol as struct pass-by-value and struct
  return — universal struct-copy mechanism.
- `1954` (**accumulating multiple fn calls**):
  `sqr(1) + sqr(2) + ... + sqr(5)` uses a **stack-
  based accumulator**:
  ```
  push 1 / call sqr / pop                  ; ax = 1
  push ax / push 2 / call sqr / pop        ; ax = 4
  mov dx, ax / pop ax / add ax, dx         ; ax = 5 (1+4)
  push ax / push 3 / call sqr / pop        ; ax = 9
  mov dx, ax / pop ax / add ax, dx         ; ax = 14 (5+9)
  ...
  ```
  Each new result pushed temporarily, then
  retrieved and summed with the new call's
  result. Standard left-to-right C evaluation.

**Universal struct-copy thresholds**:
| Operation | ≤4B | >4B |
|-----------|------|-----|
| Pass-by-value | Inline pushes (reverse mem-order) | N_SPUSH@ |
| Return | DX:AX | Hidden dest ptr + N_SCOPY@ × 2 |
| Assignment | Inline mov-pair | N_SCOPY@ |

For the Rust reimplementation:
- ≤4B struct asg: emit 2 register-mov pairs.
- >4B struct asg: emit N_SCOPY@ call with dest/src
  far ptrs + size in CX.
- Multi-call accumulation: use AX as running
  accumulator, save via push before each call.

## Int→long via cwd (signed) vs zero-fill (unsigned); nested struct = flat layout; `>>16` = high half

Fixtures `1949` (int→long signed), `1950` (nested
struct), `1951` (unsigned int→long zero-ext)
document type-aware casts and nested-struct
layout.

- `1949` (**signed int→long uses cwd**): `(long)x`
  for signed int x emits **`cwd`** to sign-extend
  AX → DX:AX:
  ```
  mov ax, [x]
  cwd                  ; sign-extend to dx:ax
  mov [y.hi], dx
  mov [y.lo], ax
  ```
  For x = -5 (0xFFFB), DX becomes 0xFFFF (sign-
  extended -1), preserving the negative value as
  -5L = 0xFFFFFFFB.
  
  Also: `(int)(y >> 16)` is **recognized at parse
  time as a direct read of `y.hi`** — no shift
  instruction emitted. The high half is just
  accessed at `[bp+disp+2]`.
- `1950` (**nested struct flat layout**):
  ```c
  struct Outer { int n; struct Inner {int x; int y;} inner; };
  ```
  lays out as:
  | Field | Offset |
  |-------|--------|
  | `o.n` | 0 |
  | `o.inner.x` | 2 |
  | `o.inner.y` | 4 |
  Total: 6 bytes. Nested struct fields use
  **summed offsets** (outer field offset + inner
  field offset). No special wrapping or alignment.
- `1951` (**unsigned int→long uses zero-fill**):
  `(long)unsigned_x` does NOT use cwd — it does
  **explicit zero-fill of the high half**:
  ```
  mov ax, [x]
  mov [y.hi], 0         ; zero-fill, NOT cwd
  mov [y.lo], ax
  ```
  For x = 0x8000, zero-fill gives y = 0x00008000
  (correct unsigned promotion). Sign-extend would
  have given 0xFFFF8000 (wrong for unsigned).
  
  Then `y >> 8` uses **N_LXRSH@** (long unsigned
  right shift helper) since the long type tracks
  the originating unsigned signedness.

**Int↔long cast summary**:
| Cast | Mechanism |
|------|-----------|
| signed int → long | `cwd` sign-extend (DX = AX's sign) |
| unsigned int → long | Zero-fill via `mov [hi], 0` |
| long → int (truncate) | Just use low half (`y.lo`) |
| `(int)(y >> N*16)` where N=1 | Direct read of `y.hi` (no shift) |

For the Rust reimplementation:
- Track signedness through casts; emit cwd for
  signed promotion, zero-fill for unsigned.
- Recognize `>> 16` truncations as direct half
  access.
- Nested struct fields: sum outer-field-offset +
  inner-field-offset for the final access offset.

## Long ABI: stored little-endian halves; args pushed hi-first; arr stride 4B per elem

Fixtures `1946` (fn returning long), `1947` (mixed
int/long args), `1948` (array of long) document
the complete long ABI.

- `1946` (**long storage and return**): a long
  value `x` in memory has the **low half at lower
  address**:
  - `x.lo` at `[bp+disp]` (lower address)
  - `x.hi` at `[bp+disp+2]` (higher address)
  
  Return convention: **DX:AX = high:low** halves.
  Function stores intermediate result into local
  long then loads back into DX:AX from the
  correct positions.
  
  Long expression codegen can have peculiar
  intermediate orderings — BCC sometimes computes
  with swapped semantics in DX:AX and uses the
  local storage as a swap-staging buffer. The
  final return convention is restored.
- `1947` (**mixed int/long args**): long arg
  passed as **two word pushes, hi FIRST then
  lo**. So the call site for `mix(1, 100L, 1000)`:
  ```
  push 1000        ; c (rightmost int)
  push 0           ; b.hi (long high half)
  push 100         ; b.lo (long low half — LAST pushed of b)
  push 1           ; a (leftmost int)
  call _mix
  add sp, 8        ; 4 args × 2 bytes
  ```
  In callee:
  - `[bp+4]` = a
  - `[bp+6]` = b.lo (long low half — closer to bp)
  - `[bp+8]` = b.hi
  - `[bp+10]` = c
  Long arg occupies **4 consecutive bytes** with
  lo at lower offset.
- `1948` (**array of long**): each element is **4
  bytes** with low half at lower address:
  - `a[0]` at `[bp-12..bp-9]`: `.lo` at -12, `.hi`
    at -10
  - `a[1]` at `[bp-8..bp-5]`: `.lo` at -8, `.hi`
    at -6
  - `a[2]` at `[bp-4..bp-1]`: `.lo` at -4, `.hi`
    at -2
  Stride = 4 bytes per element. Standard array
  layout extended for 4-byte type.

**Long memory and ABI summary**:
| Aspect | Detail |
|--------|--------|
| Memory storage | Low half at lower addr (little-endian halves) |
| Register return | DX:AX = high:low |
| Arg push order | Hi pushed FIRST, lo pushed last |
| Arg slot in callee | Lo at lower offset, hi at higher offset |
| Array stride | 4 bytes per long element |

For the Rust reimplementation:
- All long operations use little-endian halves
  in memory and stack frames.
- Long arg push: emit hi-push then lo-push (so
  lo ends at lower offset in callee).
- Long return: emit DX = high, AX = low at fn
  end.

## while-cond fn call; arg-is-cmp materializes via bool template; `x-2` uses add-imm not dec×2

Fixtures `1943` (`while (fn() < 5)`), `1944`
(`fn(a == c)`), `1945` (recursive fib) cover
remaining mixed-context shapes.

- `1943` (**while-cond with fn call**): empty
  body but explicit `jmp test` at top:
  ```
  jmp test           ; while-top-test init jmp
  body:
    ; empty
  test:
    call _read_inc
    cmp ax, 5
    jl body          ; loop while ax < 5
  ```
  Body has no instructions, but the structure
  still emits the init jmp + body label + test.
  Standard while encoding.
- `1944` (**`fn(a == c)` arg-is-cmp**): the
  comparison `a == c` is evaluated in **value
  context** (because it's an arg), so the full
  bool-materialization template emits:
  ```
  mov ax, [a] / cmp ax, [c]
  jne L_false
  mov ax, 1 / jmp end
  L_false: xor ax, ax
  end: push ax           ; AX = 0 or 1
  call _print
  ```
  Confirms: comparisons in arg position always
  materialize into AX before push.
- `1945` (**recursive fib**): standard recursive
  call pattern. Notable: **`x - 2` uses `add ax,
  -2`** (3 bytes via AX-form imm16, `05 fe ff`),
  **NOT `dec ax / dec ax`** (would be 2 bytes).
  
  So BCC's `±1` → `inc`/`dec` optimization is
  **only for value exactly ±1**, never combined
  for ±2 or larger constants. `x - 2` is treated
  as a single arithmetic op via `add ax, -2`.

**`x ± K` encoding for AX target**:
| K value | Encoding | Bytes |
|---------|----------|-------|
| +1 | `inc ax` | 1 |
| -1 | `dec ax` | 1 |
| +2..127 | `add ax, K` AX-form imm8-sext or imm16 | 3 |
| -2..-128 | `add ax, K` (encoded as imm16 negative) | 3 |
| > 127 or < -128 | `add ax, imm16` | 3 |

For ±1 in AX: 1 byte. For everything else: 3 bytes
via AX-form. No `dec×2` optimization.

For the Rust reimplementation:
- Empty while body: still emit `jmp test / body
  label / test label` skeleton.
- Comparisons in arg position: materialize via
  bool template (cmp/jcc/mov1/jmp/xor).
- `x ± 1`: emit inc/dec. Everything else: emit
  add with the signed-value imm16.

## Compound `<<=` shifts reg directly; `*=` round-trips AX; `&=` uses `81 /4 reg imm16`

Fixtures `1940` (`x <<= 3`), `1941` (`x *= 7`),
`1942` (`x &= 0xFF`) cover compound-assignment
codegen for enregistered variables.

- `1940` (**`x <<= 3` on register**): emits 3
  unrolled `shl si, 1` directly on the register
  — no AX roundtrip:
  ```
  shl si, 1            ; d1 e6
  shl si, 1            ; d1 e6
  shl si, 1            ; d1 e6
  ```
  Same N≤3 unroll / N≥4 CL-form rule applies.
  Targeting a register directly is more compact
  than going through AX.
- `1941` (**`x *= 7` on register**): mul requires
  AX, so a roundtrip:
  ```
  mov dx, 7            ; ba 07 00
  mov ax, si           ; reg → AX
  imul dx              ; AX *= DX
  mov si, ax           ; AX → reg
  ```
  No way to mul a register by an immediate without
  using AX/DX on 8086. Roundtrip cost: 4 extra
  bytes vs in-place would be.
- `1942` (**`x &= 0xFF` on register**): emits
  **`81 /4 reg imm16`** (4 bytes) directly:
  ```
  and si, 0xFF         ; 81 e6 ff 00
  ```
  No AX form needed since the generic `81 /N`
  encoding works for any 16-bit register. Result
  stays in the target register.

**Compound-assignment on register summary**:
| Op | Encoding | Bytes |
|----|----------|-------|
| `<<=` (≤3) | `shl reg, 1` × N | 2N |
| `<<=` (≥4) | `mov cl, K / shl reg, cl` | 4 |
| `*=` (any K) | `mov dx,K / mov ax,reg / imul dx / mov reg,ax` | 9+ |
| `+=` (±1) | `inc reg` or `dec reg` | 1 |
| `+=` (other) | `add reg, imm` (imm8-sext if fits) | 3 or 4 |
| `&=`/`|=`/`^=` | `81 /N reg imm16` | 4 |

For the Rust reimplementation:
- Track variable's location (register vs memory)
  per use; emit register-direct forms when
  possible.
- Mul-assign always uses AX/DX roundtrip on 8086.
- Bitwise compound assigns use generic `81 /N`
  encoding.

## Signed mod uses idiv (read DX); mul by neg uses two's-comp; zero-test: `or reg` vs `cmp [m], 0`

Fixtures `1937` (signed mod pow2), `1938` (mul by
negative), `1939` (cmp `x > 0` memory) confirm
edge cases.

- `1937` (**signed mod uses idiv, NOT AND**):
  signed `x % 4` cannot use the AND-with-(N-1)
  trick (that's only correct for unsigned). BCC
  emits the **full idiv sequence**:
  ```
  mov ax, [x]
  mov bx, 4
  cwd
  idiv bx
  mov [r], dx       ; remainder in DX
  ```
  For `-5 % 4`: -5 % 4 = -1 (truncated toward
  zero) per C89 semantics. DX holds -1. Confirms:
  the AND-pow2 optimization is UNSIGNED-only.
- `1938` (**mul by negative**): `x * -3` uses
  same pattern: `mov dx, -3 / imul dx`. The
  negative constant stored as its 16-bit two's-
  complement (`fffd`). Result low 16 bits same
  for signed and unsigned semantics.
- `1939` (**cmp `x > 0` memory operand**):
  ```
  cmp word [x], 0       ; 83 7e fe 00 (imm8-sext 0, 4 bytes)
  jle L_false            ; 7e 05 (signed inverse)
  ```
  Notable: in-memory zero-test uses `cmp [m], 0`
  (4 bytes) because `or [m], [m]` doesn't exist;
  only register-or-register shortcut.

**Zero-test encoding hierarchy**:
| Operand location | Encoding | Bytes |
|------------------|----------|-------|
| In register (e.g., SI) | `or si, si` | 2 |
| In memory | `cmp word [m], 0` | 4 |
| In AX | `or ax, ax` | 2 |

So enregistered variables get the cheaper 2-byte
zero-test via `or reg, reg`; in-memory variables
use 4-byte `cmp [m], imm8-sext-0`.

For the Rust reimplementation:
- Signed mod: always go through idiv + DX (no
  AND optimization).
- Negative const mul: 16-bit two's-complement
  encoding, same `imul` instruction.
- Zero-test: pick `or reg, reg` if value is in a
  register, else `cmp [m], 0`.

## Mul imm: `mov dx,K / imul dx`; UNSIGNED mod pow2 → `and ax, N-1`; signed div via cwd+idiv

Fixtures `1934` (mul by 12345), `1935` (unsigned
mod 8), `1936` (signed div by 13) cover the
mul/div/mod codegen for non-trivial constants.

- `1934` (**mul by large imm**): emits **`mov
  dx, imm16 / imul dx`** (5 bytes):
  ```
  mov ax, [x]
  mov dx, 12345        ; ba 39 30
  imul dx              ; f7 ea
  ```
  No strength reduction except for pow2 (which
  uses `shl`). Constant goes through DX register.
- `1935` (**unsigned mod pow2 = AND**): a key
  optimization! For unsigned `x % pow2`, BCC
  emits **`and ax, (pow2 - 1)`** instead of
  div/idiv:
  ```
  mov ax, [x]
  and ax, 7            ; 25 07 00 — for x % 8
  ```
  Mod-by-pow2 mapping:
  - `x % 2`  → `and ax, 1`
  - `x % 4`  → `and ax, 3`
  - `x % 8`  → `and ax, 7`
  - `x % 16` → `and ax, 15`
  - etc.
  
  **Only safe for UNSIGNED** — signed mod's sign
  depends on the dividend's sign, so simple AND
  doesn't work (would give wrong result for
  negatives). For signed, BCC uses `cwd / idiv`
  and reads DX for the remainder.
- `1936` (**signed div by non-pow2 const**):
  ```
  mov ax, [x]
  mov bx, 13           ; bb 0d 00 — divisor in BX
  cwd                  ; sign-extend AX → DX:AX
  idiv bx              ; f7 fb (signed div by r16)
  ```
  Total: 5 bytes. The divisor must go into a
  register since `idiv` has no immediate form.
  Result in AX (quotient), DX has remainder.

**Complete mul/div/mod summary**:
| Op | Strategy |
|----|----------|
| `* pow2` | `shl` (unrolled ≤3 or CL form ≥4) |
| `* K` (non-pow2) | `mov dx, K / imul dx` |
| unsigned `/ pow2` | `shr` (unrolled ≤3 or CL form ≥4) |
| signed `/ K` | `mov bx, K / cwd / idiv bx` |
| unsigned `/ K` (non-pow2) | `mov bx, K / xor dx,dx / div bx` |
| signed `% K` | (same as signed div, read DX) |
| **unsigned `% pow2`** | **`and ax, (pow2-1)`** ← optimization! |
| unsigned `% K` (non-pow2) | div + DX |

For the Rust reimplementation:
- Mul by const K: shl for pow2, else mov dx, K +
  imul dx.
- Unsigned mod pow2: emit `and ax, (K-1)` — never
  use div for this case.
- Signed mod: always go through idiv + DX.

## strlen pattern uses byte cmp; out-param `int **pp` via `mov [si], imm`; arr-clear uses `mov [bx], 0`

Fixtures `1931` (strlen pattern), `1932` (out-
param `int**`), `1933` (array clear via loop)
cover more pointer/array idioms.

- `1931` (**strlen pattern**): both pointers
  enregister (s in DI, p in SI):
  ```
  mov di, [s]
  mov si, di            ; p = s
  jmp test
  body:
    inc si              ; p++
  test:
    cmp byte [si], 0    ; 80 3c 00 — byte cmp
    jne body            ; loop while non-zero
  mov ax, si / sub ax, di    ; p - s
  ```
  Byte compare `80 /7 disp imm8` (3 bytes for
  `[SI]` with imm0). Most compact form for
  testing the byte at a pointer. After: `p - s`
  via `sub ax, di` gives the length (since
  pointer arithmetic on byte ptrs is in bytes).
- `1932` (**out-param `int **pp`**): callee gets
  ptr-to-ptr in `[bp+4]`, stores `&storage` at
  the target:
  ```
  mov si, [pp]
  mov word [si], imm16    ; c7 04 imm16 + FIXUPP to _storage
  ```
  The `c7 04` is `mov word [si], imm16` (4
  bytes). With FIXUPP, the imm16 resolves to
  the static's address.
  
  Caller pushes `&local_p`, gets back a modified
  local_p, then derefs as usual.
- `1933` (**array clear via loop**): emits **`mov
  word [bx], 0`** per iteration (`c7 07 00 00`, 4
  bytes). No `rep stosw` or `xor + stos`
  optimization. Each iteration recomputes `&a[i]`
  via the standard `shl + lea + add` sequence.
  
  BCC's "no optimization" rule holds — `int a[5]
  = {0}` initialization gets N_SCOPY@ from zero
  data; explicit loop just emits per-element
  stores.

For the Rust reimplementation:
- strlen-style loop: enregister both ptrs, use
  byte-cmp `cmp byte [si], 0 / jne body / sub
  ax, di` epilogue.
- Out-param: `mov si, [pp] / mov [si], val`.
- Array clear via loop: per-iteration `mov word
  [bx], 0`. No `rep stos*` (not used by BCC).

## Linked-list traverse via `[bx+disp]` chain; fn ret ptr in AX; set via ptr-arg = `mov [si], val`

Fixtures `1928` (self-referential struct), `1929`
(fn returning ptr), `1930` (set via ptr arg)
cover pointer-laden patterns.

- `1928` (**linked-list traversal**):
  `struct Node {int v; struct Node *next;}` lays
  out as 4 bytes per node. `a.next->next->v`
  chains via repeated `mov bx, [bx+disp]`:
  ```
  mov bx, [a.next]      ; 8b 5e fe — load first ptr
  mov bx, [bx+2]        ; 8b 5f 02 — follow .next field
  mov ax, [bx]          ; 8b 07    — read .v field (offset 0)
  ```
  Each link in the chain = `mov bx, [bx+next_field_offset]`.
  Top-level `a.next` is **reloaded per chained-
  expression** — no CSE.
- `1929` (**fn returns ptr**): returns the
  **target's offset in AX** (16-bit in small
  model). For `return &g;` where g is a global:
  ```
  mov ax, 0             ; b8 00 00 (with FIXUPP to _g)
  ret
  ```
  Caller stores AX as a ptr and derefs via `mov
  bx, ax / mov ax, [bx]`.
- `1930` (**set via ptr arg**): callee pattern:
  ```
  mov si, [p]           ; load ptr from arg
  mov ax, [v]           ; load value
  mov [si], ax          ; 89 04 — store via ptr
  ```
  `mov [si], ax` is 2 bytes (`89 04`). The `04`
  ModR/M = mod=00 reg=AX rm=100 ([SI]).
  
  At call site:
  ```
  mov ax, 42 / push ax       ; v arg
  lea ax, [x] / push ax      ; &x arg
  call _set
  add sp, 4
  ```
  The `&x` is computed with `lea ax, [bp+disp]`
  (cheaper than separate calc), then pushed.

For the Rust reimplementation:
- Chained ptr-deref `s->f->g`: emit successive
  `mov bx, [bx+offset]` loads with each field's
  offset.
- Fn returns ptr: return offset in AX (small) or
  DX:AX (far model).
- Set via ptr arg: `mov si/bx, [p] / mov [si],
  val`. Use lea for `&local` addressing.

## 6-arg call: 4B per push; long arg = 2 word-pushes hi-first; chained calls bottom-up

Fixtures `1925` (6 args), `1926` (long arg),
`1927` (chained calls) cover remaining call-site
shapes.

- `1925` (**6 args**): each int constant arg is
  pushed via **`mov ax, imm / push ax`** (4 bytes
  per arg), pushed right-to-left. After call,
  cleanup uses **single `add sp, N*2`**:
  ```
  mov ax, 6 / push ax    ; b8 06 00 50
  mov ax, 5 / push ax
  mov ax, 4 / push ax
  mov ax, 3 / push ax
  mov ax, 2 / push ax
  mov ax, 1 / push ax
  call _sum6
  add sp, 12             ; cleanup 6 × 2 bytes
  ```
  Note: BCC does **NOT use 80186+'s `push imm16`**
  (`68 imm16`, 3 bytes) — uses 8086-compatible
  `mov + push` (4 bytes) instead. BCC targets
  8086 only.
- `1926` (**long arg**): a long value pushed as
  **two word-pushes, hi first then lo**:
  ```
  ff 76 fe        ; push [y.hi]  (higher offset pushed first)
  ff 76 fc        ; push [y.lo]
  call _truncate_long
  pop / pop       ; 4 bytes cleanup
  ```
  In callee, `[bp+4]` = lo half (last pushed),
  `[bp+6]` = hi half. Memory order: low at
  smaller offset. Matches little-endian
  representation.
- `1927` (**chained calls bottom-up**): `f(g(h(x)))`
  evaluates innermost first:
  ```
  xor ax, ax / push ax     ; x = 0
  call h                    ; ax = h(0)
  pop                      ; cleanup
  push ax                   ; new arg = h(0)
  call g                    ; ax = g(h(0))
  pop
  push ax
  call f                    ; ax = f(g(h(0)))
  pop
  ```
  Each call's result (in AX) is immediately
  pushed as the next call's arg. No deep stack
  buildup.

For the Rust reimplementation:
- Multi-arg calls: `mov ax, val / push ax` per
  arg (8086-compatible, NOT `push imm`).
- Cleanup: single `add sp, N*2` after call
  (cdecl).
- Long arg: 2 word-pushes, hi first; callee sees
  lo at [bp+4], hi at [bp+6].
- Chained calls: evaluate inner first, push
  result, then outer.

## 2D arr passes as ptr; partial init uses N_SCOPY@ from zero-padded `_DATA`; global arr in `_DATA`

Fixtures `1922` (pass 2D array), `1923` (partial
init), `1924` (global init array) cover array
initialization and passing semantics.

- `1922` (**2D array as ptr arg**): `int a[2][2]`
  parameter is just a **pointer to the start**.
  Callee accesses elements via flat indexing:
  ```
  mov si, [a]               ; load ptr to first row
  mov ax, [si]              ; a[0][0]
  add ax, [si+2]            ; a[0][1]
  add ax, [si+4]            ; a[1][0]
  add ax, [si+6]            ; a[1][1]
  ```
  Despite the source-level `[2][2]` shape, codegen
  treats it as flat — uses constant disp8 offsets
  for each element. The outer dimension decays
  to pointer; inner dimensions are baked into
  the offsets.
- `1923` (**partial init `int a[5] = {1, 2}`**):
  initializer values stored in `_DATA` with the
  rest **zero-filled** per C semantics:
  ```
  ; _DATA: 01 00 02 00 00 00 00 00 00 00  (1, 2, 0, 0, 0)
  ```
  Then **N_SCOPY@** copies the 10 bytes from
  `_DATA` to the local array on stack at fn
  entry. Same protocol as `char a[] = "ABC"`
  but for ints.
- `1924` (**global array init in `_DATA`**):
  `int table[5] = {100, 200, 300, 400, 500}` is
  stored directly in `_DATA` at file scope:
  ```
  ; _DATA: 64 00 c8 00 2c 01 90 01 f4 01
  ```
  `_table` exported in PUBDEF. Access `table[2]`
  uses **`a1 disp16`** (AX-form mov from direct
  address) with FIXUPP for table+4 offset.

So **arrays-with-init storage hierarchy**:
| Scope | Storage | Init mechanism |
|-------|---------|---------------|
| Local | Stack | N_SCOPY@ from `_DATA` template at fn entry |
| Static local | `_DATA` | Init values directly stored |
| Global | `_DATA` (or `_BSS` if zero-init) | Init values directly stored |

For the Rust reimplementation:
- 2D-array param: emit as ptr, use flat indexing
  with row*width*sizeof + col*sizeof.
- Partial init: emit zero-padded template in
  `_DATA`, emit N_SCOPY@ at fn entry.
- Global/static array init: emit directly in
  `_DATA`/`_BSS` with values.

## Bool→int = full template; do-while-cmp loops back via fwd-jcc; string table = ptrs into `_DATA`

Fixtures `1919` (bool-to-int store), `1920`
(do-while with cmp), `1921` (string table) cover
three more idioms.

- `1919` (**bool→int store uses full template**):
  `int b = (x > 0);` (value context) emits the
  full materialization:
  ```
  or si, si              ; zero-test x
  jle L_false            ; inverse jcc (NOT >  is <=)
  mov ax, 1
  jmp end
  L_false: xor ax, ax
  end: mov [b], ax
  ```
  Same template for all comparison-to-int
  assignments. Contrast with **boolean context**
  (in `if`/`while`) which just emits cmp+jcc and
  doesn't materialize.
- `1920` (**do-while-cmp loops back via fwd-
  jcc**): `do { body; i++; } while (i < 10);`
  emits:
  ```
  body:
    add di, si      ; sum += i
    inc si          ; i++
    cmp si, 10
    jl body         ; loop while i < 10
  ```
  Note: **`jl` is the forward-sense jcc** here —
  jump-if-less = continue while i < 10. Do-while
  loops back when the condition is TRUE, so the
  jcc direction is **non-inverse** (matching the
  source-level comparator).
  
  Contrast with `if (i < 10) X;` where the jcc
  for the false-branch is `jge` (inverse).
- `1921` (**string table = ptrs into `_DATA`**):
  `char *table[3] = ...` assigns FIXUPP'd offsets
  to slots. The strings themselves are stored
  consecutively in `_DATA`:
  ```
  ; data:  AB\0CD\0EF\0  at offsets 0, 3, 6
  c7 46 fa 00 00         ; table[0] = "AB"@0
  c7 46 fc 03 00         ; table[1] = "CD"@3
  c7 46 fe 06 00         ; table[2] = "EF"@6
  ```
  Access `table[1][0]` is 2-step: load ptr from
  slot, then deref the ptr.

**Boolean vs value context revisited**:
| Context | Codegen |
|---------|---------|
| boolean (if/while/for cond) | cmp + jcc directly |
| value (assigned, returned) | cmp/jcc + mov ax,1/jmp/xor ax,ax template |

For the Rust reimplementation:
- Track expression context (boolean vs value); use
  different lowering for comparisons.
- do-while bottom-test uses fwd-sense jcc; while-
  top-test uses inverse-sense jcc.
- String table = array of offset-FIXUPPs into
  consecutive strings in `_DATA`.

## Static fn = internal linkage no PUBDEF; string concat parse-time; arr of fn-ptr = word slots

Fixtures `1916` (static fn), `1917` (string
literal concatenation), `1918` (array of fn ptrs)
cover linkage, parser, and indirect-call shapes.

- `1916` (**static fn = internal linkage**):
  `static int hidden(int x) { ... }` is **NOT
  listed in PUBDEF** — only `_main` appears in
  the exported symbols. The call from main to
  hidden uses a **direct relative near-call**
  (`e8 disp`) with no FIXUPP, since both are in
  the same OBJ.
  
  Other TUs cannot reference the static fn (no
  symbol exported for linker). Standard C internal-
  linkage semantics.
- `1917` (**string literals concatenate at parse**):
  `"Hello, " "World!"` becomes a **single string
  literal** "Hello, World!" in `_DATA`. The lexer
  handles the concatenation; codegen sees one
  combined literal.
- `1918` (**array of fn pointers**): `int
  (*fns[2])(int)` allocates **2 word slots** on
  stack:
  ```
  c7 46 fc 00 00         ; fns[0] = _add1 (FIXUPP)
  c7 46 fe 0b 00         ; fns[1] = _add2 (FIXUPP)
  ; call fns[0](10):
  mov ax, 10 / push ax
  call near [bp+fc]       ; ff 56 fc — indirect via slot
  ```
  Each call uses `ff 56 disp` (call near [bp+disp])
  with the slot's stack offset. Same indirect-
  call opcode as for any fn-ptr access.

For the Rust reimplementation:
- `static` linkage: omit from PUBDEF; internal-
  only symbol table entry.
- String literal concatenation: handle in lexer,
  combine adjacent string tokens into single
  literal before codegen.
- Array of fn-ptrs: each slot is a near-ptr (2B
  in small model, 4B in large); call via `ff 56
  disp` indirect-call.

## Switch on long: two-phase search-table; struct-of-6 uses `imul`; signed `>=` uses `jl`

Fixtures `1913` (switch on long), `1914` (array
of 6-byte struct), `1915` (signed `>=` cmp) cover
remaining shapes.

- `1913` (**switch on long = 2-phase search**):
  switch on `long x` (32-bit) uses the linear-
  search-table strategy with **two-phase compare**
  per iteration:
  ```
  loop_top:
  cs: mov ax, [bx]          ; load case.lo
  cmp ax, [temp.lo]
  jne L_next                 ; lo mismatch → next
  cs: mov ax, [bx+6]         ; load case.hi (table offset = 6 bytes for 3 cases × 2)
  cmp ax, [temp.hi]
  je L_found                 ; both match → use this case
  L_next:
  inc bx / inc bx
  loop loop_top              ; CX-- + jump if non-zero
  jmp L_after
  L_found:
  cs: jmp [bx + 12]          ; offset to body-target table (12 = 2 × 6)
  
  ; THREE tables in code segment:
  case_lo_table:     dw 1, 2, 3
  case_hi_table:     dw 0, 0, 0
  body_offset_table: dw L_c1, L_c2, L_c3
  ```
  Long switches **always use search-table**
  (never jump-table) — a 32-bit-indexed jump
  table would be impractical.
- `1914` (**non-pow2 struct stride uses imul**):
  `struct R {int a; int b; int c;}` (sizeof = 6)
  arrays use **`imul`** for index computation:
  ```
  mov ax, i / mov dx, 6 / imul dx     ; ax = i * 6
  lea dx, [base + field_offset]       ; field-specific base
  add ax, dx                          ; final addr
  mov bx, ax / mov [bx], ...
  ```
  No CSE across fields in the same iteration —
  each field access recomputes `i * 6` via imul.
  Pow2 strides use shl (cheaper); non-pow2
  always uses imul.
- `1915` (**signed `>=` uses `jl`**): false-
  branch jcc for `a >= b` is `jl` (`0x7C`,
  signed jump-if-less). Inverse of `>=` is `<`,
  hence `jl`.

**Complete signed-cmp jcc table** (false-branch):
| Op | False-jcc | Opcode |
|----|-----------|--------|
| `<` | `jge` | 7D |
| `<=` | `jg` | 7F |
| `>` | `jle` | 7E |
| `>=` | `jl` | 7C |
| `==` | `jne` | 75 |
| `!=` | `je` | 74 |

For the Rust reimplementation:
- Long switch: always emit search-table with two-
  phase compare. Three tables (case.lo, case.hi,
  body-target) of `N * 2` bytes each in code
  segment.
- Array of struct with non-pow2 stride: `mov ax,
  idx / mov dx, sizeof / imul dx`.
- Track operand signedness, choose jcc per
  signedness × operator matrix.

## Default doesn't count toward N; 2 cases linear; imm16 base uses `81 eb`

Fixtures `1910` (3 cases + default), `1911` (2
cases only), `1912` (base 200 imm16) refine the
switch case-count and base-encoding rules.

- `1910` (**3 cases + default = linear chain**):
  the **4-case threshold counts only explicit
  cases** — default doesn't push it over. 3
  explicit cases + default still uses linear
  cmp/je with `jmp L_default` as final fallback.
- `1911` (**2 cases = linear**): confirms 2 cases
  uses cmp/je chain (well below threshold).
- `1912` (**base 200 → imm16 sub**): when base
  doesn't fit imm8-sext, BCC uses **`81 eb imm16`**
  (sub r16, imm16, 4 bytes):
  ```
  mov bx, [x]
  sub bx, 200            ; 81 eb c8 00 (imm16 form, 4 bytes)
  cmp bx, 3
  ja L_after
  ; ... rest same as before
  ```

**Final base-normalize encoding hierarchy**:
| Base value | Encoding | Bytes |
|------------|----------|-------|
| 0 | (omitted) | 0 |
| 1 | `dec bx` | 1 |
| -128..127 (≠ 0,1) | `83 eb imm8` | 3 |
| else | `81 eb imm16` | 4 |

For the Rust reimplementation:
- Threshold for jump-table: count explicit cases
  (NOT default).
- Base-normalize: 0 omit, 1 dec, fits imm8-sext
  sub-imm8, else sub-imm16.

## NEW: 3rd switch strategy — linear-search table for sparse N ≥ 4 cases (uses `loop` insn)

Fixtures `1907` (base 0), `1908` (huge-gap),
`1909` (negative base) uncover a **third switch
codegen strategy** plus boundary refinements.

- `1907` (**base = 0 omits subtract**): when the
  lowest case value is 0, BCC **omits the
  normalization step** entirely:
  ```
  mov bx, [x]
  cmp bx, 3            ; bounds (no sub needed, x already 0-based)
  ja L_after
  shl bx, 1
  cs: jmp [bx + table]
  ```
  Most compact form (4 fewer bytes than base≠0).
- `1908` (**large gap → linear-search TABLE**):
  cases 1, 2, 3, 50 (huge gap) use a **brand new
  strategy** — neither jump table nor cmp/je chain:
  ```
  mov [tmp], x
  mov cx, 4              ; loop count = N cases
  mov bx, value_table    ; ptr to case-value table
  loop_top:
  cs: mov ax, [bx]       ; load case value
  cmp ax, [tmp]
  je L_found             ; match → use this case
  inc bx / inc bx        ; bx += 2 (next value)
  loop loop_top          ; CX-- + jump if non-zero
  jmp L_default          ; no match
  L_found:
  cs: jmp [bx + 8]       ; bx points into value_table;
                          ; +8 = offset to corresponding target_table entry
  
  ; two parallel tables in code segment:
  value_table:  dw 1, 2, 3, 50      ; the case values
  target_table: dw L_c1, L_c2, L_c3, L_c50  ; corresponding bodies
  ```
  The `bx + 8` trick: BX iterates the value-
  table; once matched, the matching index also
  identifies the target-table entry at offset
  `+sizeof(value_table)` (= 8 bytes for 4 cases).
  
  Uses the **`loop` instruction** (`e2 rel8`) for
  compact CX-counted iteration. This is a
  **third codegen strategy** for switch:
  - Linear cmp/je chain (for ≤ 3 cases or sparse-
    few)
  - Jump table (≥ 4 cases, range small)
  - **Linear-search value table** (≥ 4 cases,
    range large)
  
  Threshold for jump-table vs linear-search-table
  not yet pinned, but ≥ ~10× more values than
  cases probably triggers the switch.
- `1909` (**negative base**): cases -2 to 1 (base
  -2) uses `sub bx, -2` encoded as `83 eb fe`
  (imm8-sext to 0xFFFE = -2). Negative bases work
  correctly via sign-extension semantics of
  imm8-sext.

**Three switch strategies summary**:
| Pattern | Detection | Lowering |
|---------|-----------|----------|
| Linear chain | N ≤ 3 OR sparse with small N | cmp ax, K / je ... per case |
| Jump table | N ≥ 4, range small | sub/dec normalize, bounds, shl, cs:jmp [table] |
| Search table | N ≥ 4, range large | cx-count loop over value_table, cs:jmp [bx+offset] |

For the Rust reimplementation:
- Add the linear-search-table strategy to switch
  lowering.
- Base = 0: omit normalization step.
- Base < 0: use `sub bx, K` (imm8-sext handles
  negative).

## Switch jump-table: gaps point to L_after; default = `ja L_default`; non-1 base uses `sub bx, K`

Fixtures `1904` (4 cases with gap), `1905` (4
cases + default), and `1906` (cases starting at
base 5) refine the jump-table mechanism.

- `1904` (**gap in cases**): `case 1, 2, 4, 5`
  (with case 3 missing) **still uses jump table**
  — bounds check is `cmp bx, 4` (range = max -
  min = 5-1 = 4), table has **5 entries**:
  | Slot | Case | Target |
  |------|------|--------|
  | 0 | 1 | case 1 body |
  | 1 | 2 | case 2 body |
  | 2 | (missing 3) | **L_after** |
  | 3 | 4 | case 4 body |
  | 4 | 5 | case 5 body |
  
  Missing-case slots **point to L_after**
  (past the switch) — equivalent to "no match,
  fall through". So small gaps don't disable the
  jump-table approach.
  
  Refined threshold: N ≥ 4 distinct cases AND
  the case-value range is dense enough (gap-
  tolerance threshold not yet pinned).
- `1905` (**4 cases + default**): table has 4
  entries (one per explicit case); the **bounds
  check's `ja` targets the default body**
  directly:
  ```
  cmp bx, 3
  ja L_default       ; out-of-range → default
  shl bx, 1
  cs: jmp [bx + table]
  ```
  Default body laid out after case bodies, with
  its own break.
- `1906` (**non-1 base case value**): `case 5, 6,
  7, 8` uses **`sub bx, 5`** (`83 eb 05`, 3
  bytes) instead of `dec bx`. The bounds check
  and jump-table protocol are otherwise
  identical:
  ```
  mov bx, [x]
  sub bx, 5            ; normalize to 0-based
  cmp bx, 3            ; bounds (range = 3)
  ja L_after
  shl bx, 1
  cs: jmp [bx + table]
  ```
  
  Encoding choice:
  - base = 1: `dec bx` (1B)
  - base = K (≠ 1, fits imm8-sext): `sub bx, K`
    (3B)
  - base = K (imm16 only): `sub bx, K` (4B via
    `81 eb`)

For the Rust reimplementation:
- Switch jump-table mechanism:
  1. Compute base = min case value
  2. Subtract base from input (dec for base=1, sub
     bx, base otherwise)
  3. Bounds check `cmp bx, (max-min)` + `ja
     <default or L_after>`
  4. `shl bx, 1`
  5. `cs: jmp [bx + table]`
  6. Table fills gaps with L_after (or default if
     present)
- Default present: `ja` target = default body
- Default absent: `ja` target = L_after

## Switch jump-table threshold pinned: N ≥ 4 contiguous cases → jump table

Fixtures `1901` (4 cases), `1902` (5 cases), and
`1903` (6 cases) pin down the exact threshold for
jump-table-based switch lowering.

All three use **jump tables** with the same
template:
```
mov bx, [x]
dec bx                  ; bx -= 1 (lowest case)
cmp bx, N-1             ; bounds check (N = case count)
ja L_after              ; out-of-range
shl bx, 1               ; word index
cs: jmp [bx + table]    ; indirect jump

; data segment (in code):
table: dw L_case1, L_case2, ..., L_caseN
```

The bounds check uses N-1 as the upper limit:
- 4 cases: `cmp bx, 3`
- 5 cases: `cmp bx, 4`
- 6 cases: `cmp bx, 5`
- 8 cases: `cmp bx, 7`

Combined with earlier findings:
| N cases | Codegen |
|---------|---------|
| 3 (1894) | Linear cmp/je chain |
| 4 (1901) | Jump table |
| 5 (1902) | Jump table |
| 6 (1903) | Jump table |
| 8 (1898) | Jump table |
| 3 sparse (1897) | Linear cmp/je chain |

**Exact threshold**: **N ≥ 4 contiguous cases →
jump table**. For N ≤ 3 OR sparse cases, linear
chain is used.

The contiguity requirement is critical: even with
N ≥ 4, if cases are sparse (e.g., 10, 100, 1000),
the jump table would have huge gaps and BCC falls
back to linear chain.

For the Rust reimplementation:
- Analyze case-value distribution:
  - If N ≥ 4 AND cases form a contiguous range
    (max - min == N - 1): emit jump table
  - Else: emit linear cmp/je chain
- Jump-table emission:
  - `mov bx, [x] / sub bx, min / cmp bx, N-1 /
    ja default / shl bx, 1 / cs: jmp [bx +
    table_offset]`
  - Table in code segment with N word entries
- Update [[batch-525-switch]] reference: that
  earlier note's "always linear" claim was wrong
  for N ≥ 4 contiguous.

## CORRECTION: switch with 8 contiguous cases uses JUMP TABLE; char-switch promotes via cbw; static arr in `_DATA`

Fixtures `1898` (8 contiguous cases), `1899`
(switch on char), and `1900` (static array
lookup) overturn the earlier "all switches use
linear chain" claim and document table-driven
switch.

- `1898` (**8 cases use JUMP TABLE** — overturns
  prior finding):
  ```
  mov bx, [x]
  dec bx                ; bx = x - lowest_case (1 here)
  cmp bx, 7             ; bounds: 8-1
  ja L_after            ; out-of-range → skip switch
  shl bx, 1             ; bx *= 2 (word index)
  cs: jmp [bx + table]  ; INDIRECT JUMP via table
  
  ; data in code segment:
  table: dw L_case1, L_case2, L_case3, ..., L_case8
  ```
  This **overturns** the claim from
  [[batch-525-switch]] that BCC always uses
  linear chain. **BCC uses a jump table for
  dense contiguous cases with sufficient count**.
  
  Threshold for jump table is **somewhere
  between 4 and 8** cases (not yet pinned down).
  Sparse cases (1897) and few cases (1894 with
  3) still use linear chain.
  
  The jump table is stored **in the code segment**
  (CS:), with each entry being a 2-byte offset
  to the case body. The bounds check before the
  table dispatch handles out-of-range values
  (including those past the highest case).
- `1899` (**char-switch promotes via cbw**):
  switch on a `char` variable emits:
  ```
  mov al, [c]
  cbw                  ; sign-extend AL → AX
  ; then cmp ax, K / je ... for each case
  ```
  Char arg is promoted to int (signed sign-
  extension) before the switch's cmp. With 3
  cases here, linear chain is used (below jump-
  table threshold).
- `1900` (**static array in `_DATA`**): `static
  int table[5] = {10, 20, 30, 40, 50}` is
  emitted in `_DATA` with the initial values:
  ```
  table: dw 10, 20, 30, 40, 50    ; 0a 00 14 00 1e 00 28 00 32 00
  ```
  Accessed via `mov ax, [bx + table]` (with
  FIXUPP for table's address). Same codegen
  as a static array at file scope.

**Revised switch lowering rules**:
| Case shape | Codegen |
|------------|---------|
| Few cases (≤ ~4) | Linear cmp/je chain |
| Many sparse cases | Linear cmp/je chain |
| Many DENSE contiguous cases | Jump table |

For the Rust reimplementation:
- Switch lowering: analyze case-value distribution
  - If dense+contiguous and count ≥ N (threshold
    TBD, likely 4-5): emit jump table in CS
  - Else: emit linear cmp/je chain
- Jump table: subtract base, bounds check, shl bx
  1, `cs: jmp [bx + table_offset]`.
- Char switch: cbw before cmp (sign-extend to int).
- Static array initializers: place in `_DATA`
  with values at FIXUPP-resolved offsets.

## Switch fallthrough = same target; no-default = direct `jmp end`; sparse cases linear too

Fixtures `1895` (case fallthrough), `1896` (no
default), and `1897` (sparse cases) complete the
switch picture.

- `1895` (**case fallthrough = same target**):
  ```c
  case 1:
  case 2:
    r = 10; break;
  ```
  Both `case 1` and `case 2` get `je L_case1_2`
  pointing at the **same body**. No code
  duplication; just multiple labels for one body.
  Most efficient possible representation of
  fallthrough.
- `1896` (**switch without default = jmp end**):
  after all case tests:
  ```
  cmp ax, 1 / je L_case1
  cmp ax, 2 / je L_case2
  jmp L_end          ; no default: skip directly to end
  ```
  No default body, just skip past.
- `1897` (**sparse cases use linear chain too**):
  `case 10: ... case 100: ... case 1000:` emits
  the same cmp/je sequence regardless of value
  spread. Case constants encoded as imm16 (e.g.,
  `3d e8 03` for cmp ax, 1000). No jump-table
  specialization even for large gaps.

So **all switch shapes use linear cmp/je chain**:
- Fallthrough: shared body via multiple jcc → same target
- No default: skip past via `jmp end`
- Sparse / dense / few / many cases: all linear

This is consistent with BCC's "compile each case
independently" approach. No advanced
specialization. Per-case overhead is constant:
~5 bytes (3 cmp + 2 je) regardless of value
density.

For the Rust reimplementation:
- All switches lower to linear cmp/je chains.
- Fallthrough: emit single body, multiple labels
  pointing at it.
- No default: `jmp end` after the chain.
- Case constants always imm16 (or AX-form `3d`).

## `goto` doesn't fuse with `if`; dead code after return emitted; `switch` = linear cmp/je chain

Fixtures `1892` (goto label), `1893` (dead code
after return), and `1894` (switch statement)
finalise the control-flow catalog.

- `1892` (**`if (cond) goto X;` no fusion**): like
  `if (cond) break;`, BCC compiles as:
  ```
  cmp si, 5
  jge L_skip      ; inverse-jcc
  jmp loop        ; the goto (unconditional)
  L_skip:
  ```
  Could have been `jl loop` (one instruction) but
  BCC keeps the cmp/jcc/jmp pattern per its "each
  statement independent" rule.
- `1893` (**dead code after return is emitted**):
  ```c
  return x;       // first return
  x = 99;         // dead
  return x + 1;   // dead
  ```
  compiles to:
  ```
  mov ax, si
  jmp epilogue     ; first return
  ; DEAD CODE follows:
  mov si, 99
  mov ax, si / inc ax
  epilogue:
  pop si / pop bp / ret
  ```
  BCC emits the dead code into the OBJ. No DCE.
  Confirms: **BCC 2.0 performs zero optimizations**
  beyond syntactic constant folding.
- `1894` (**switch = linear cmp/je chain**): no
  jump table; each case becomes a cmp+je pair:
  ```
  mov ax, [x]
  cmp ax, 1 / je L_case1
  cmp ax, 2 / je L_case2
  cmp ax, 3 / je L_case3
  jmp L_default
  L_case1: ...; jmp L_end
  L_case2: ...; jmp L_end
  L_case3: ...; jmp L_end
  L_default: ...; jmp L_end
  L_end:
  ```
  Per-case cost: 5 bytes (3 cmp + 2 je). The
  `break` after each case becomes `jmp L_end`.
  Default goes to its own block. All case bodies
  share a single L_end target.
  
  This is simple but suboptimal for dense
  enum-like switches. A jump table (`jmp [table +
  case*2]`) would be O(1) and smaller for >4
  cases, but BCC always uses the linear chain.

So **BCC's compilation philosophy** is:
1. Compile each statement independently — no
   peephole fusion with surrounding context
2. No dead-code elimination — emit what the source
   specifies, including unreachable code
3. No control-flow optimisations — switches are
   linear chains, not jump tables; if-break/if-
   goto get inverse-condition compilation
4. Only syntactic constant folding (compile-time
   constants in expressions); no algebraic
   simplification, no CSE, no DCE

This makes BCC's codegen **highly predictable** —
each source statement maps to a small, fixed
sequence of instructions. Easy to byte-exact
reproduce.

## Empty fn keeps prologue/epilogue; `while(1)+break` no fusion; `continue` = goto update

Fixtures `1889` (empty function), `1890` (while(1)
with break), and `1891` (continue in for) cover
remaining control-flow shapes.

- `1889` (**empty function keeps prologue/
  epilogue**): `void noop(void) { }` still emits
  `push bp / mov bp, sp / pop bp / ret` (4 bytes
  total). No leaf-function optimization to skip
  BP setup.
- `1890` (**`while(1) + break` no fusion**):
  `while (1) { body; if (cond) break; }` emits
  ```
  top:
    body
    cmp [v], k
    jne L_skip          ; if NOT (cond), skip the break
    jmp L_break         ; the break
  L_skip:
    jmp top             ; loop back
  L_break:
    ; after loop
  ```
  Notable: BCC does **NOT fuse** `if (cond)
  break;` into a single `je L_break`. The `break`
  is compiled as a regular goto, with the if's
  own jcc chain preserved. This is wasteful — 7
  bytes where 5 would suffice. Consistent with
  BCC's "compile each statement independently"
  philosophy.
- `1891` (**`continue` = goto update**):
  `for (init; cond; update) { ...; continue; ...; }`
  with continue lowers to **jump to the update
  step**:
  ```
  body:
    ; ... if (i & 1) continue; ...
    test si, 1
    je L_not_odd
    jmp L_continue       ; continue → skip rest of body
  L_not_odd:
    ; rest of body
  L_continue:
    inc si               ; update
  test:
    cmp si, 10
    jl body
  ```
  So `continue` is **`goto <update-label>`** —
  not `goto <test-label>`. The for-loop's update
  always runs even on continue.

For the Rust reimplementation:
- Always emit prologue/epilogue, regardless of
  function body size or content.
- `while(1)` body + break: emit body, unconditional
  jmp back; `break` jumps to past-the-loop. Don't
  attempt to fuse `if-break` patterns.
- `continue` jumps to update-clause; `break` jumps
  past the loop entirely.

## `volatile` is no-op in BCC; do-while saves init jmp; forward decl resolves at parse

Fixtures `1886` (volatile int), `1887` (do-while
with zero test), and `1888` (forward fn decl)
cover three remaining type/control-flow shapes.

- `1886` (**`volatile` is effectively no-op**):
  `volatile int x = 0; x = 1; x = 2; return x;`
  emits **all three stores** plus the load.
  Notable: a non-volatile version would emit
  **identical code** because BCC **never performs
  dead-store elimination** (or any other
  optimization that would remove the qualifier's
  purpose).
  
  So in BCC 2.0, `volatile` is a **type-system
  marker** with **zero codegen effect** — the
  compiler already preserves all side-effects by
  default.
- `1887` (**do-while saves the init jmp**):
  ```
  ; no jmp to test at top
  body:
    add di, si      ; sum += i
    dec si          ; i--
    or si, si       ; test i (zero-test shortcut)
    jne body        ; loop while non-zero
  ```
  Saves 2 bytes vs while-bottom-test pattern (no
  `jmp test` at the top, since do-while
  guarantees ≥1 iteration). The test uses cheap
  `or si, si` since condition is just a variable.
- `1888` (**forward decl + later defn**):
  ```c
  int callee(int x);
  int main(void) { return callee(7); }
  int callee(int x) { ... }
  ```
  Compiles cleanly. The forward decl provides the
  prototype; main's `call _callee` uses a forward
  relative near-call (`e8 +disp`) since both
  functions are in the same segment. Symbol
  resolution happens at parse time.
  
  Note: function order in source = order in OBJ;
  main appears before _callee in symbol table.
  The forward call's displacement is resolved
  during codegen pass when the target function's
  position is known.

For the Rust reimplementation:
- `volatile` qualifier: trackable in type system,
  no codegen change (BCC never optimized stores
  anyway).
- do-while: emit body first, test at bottom,
  conditional jump back. No init jmp.
- Forward fn decls: resolve via two-pass parse
  (collect prototypes first) or back-patch
  during codegen.

## `char a[] = "ABC"` uses N_SCOPY@; array decays to ptr at call site; sizeof dead-codes

Fixtures `1883` (char array init from string),
`1884` (array decay to ptr arg), and `1885`
(sizeof arr) cover three array-related shapes.

- `1883` (**`char a[] = "ABC"` uses N_SCOPY@**):
  string-literal init of a local char array calls
  the helper:
  ```
  push ss / lea ax, [a] / push ax    ; dest = SS:offset
  push ds / mov ax, "ABC"@ / push ax ; source = DS:offset of literal
  mov cx, 4                          ; count = 4 (3 chars + \0)
  call N_SCOPY@
  ```
  Same helper used for struct copying in [[batch-
  520-struct-return]]. The string literal is in
  `_DATA` (`ABC\0` bytes); local array allocated
  on stack; copy happens at function entry.
- `1884` (**array decay at call site**): `first(a)`
  where `a` is `int[3]` emits `lea ax, [a] / push
  ax` — just a **pointer push**. The array decays
  to `int *` per C semantics. Callee receives a
  regular pointer; uses `mov si, [p] / mov ax,
  [si]` to deref.
- `1885` (**`sizeof(a) / sizeof(a[0])` fully
  resolved at parse time**): `n = sizeof(a) /
  sizeof(a[0])` compiles to **`mov [n], 10`** —
  the division is computed at parse time (20/2 =
  10). Notably, **the array `a` is never even
  allocated** since it's only referenced in
  sizeof. Stack frame has just 2 bytes for `n`.
  
  Confirms: `sizeof` is always a compile-time
  constant; arrays referenced ONLY by sizeof are
  dead-code-eliminated.

For the Rust reimplementation:
- `char a[] = "..."` lowers to N_SCOPY@ at fn entry,
  with src = string literal in `_DATA`.
- Array-to-pointer decay: emit `lea / push` at
  call site; type system tracks the decay.
- `sizeof` evaluates to compile-time integer; arrays
  used only in sizeof can be omitted from stack.

## Bitfield byte vs word granularity; enum = int; typedef = parser-level alias

Fixtures `1880` (bitfield spans byte boundary),
`1881` (enum semantics), and `1882` (typedef
chain) cover three type-system shapes.

- `1880` (**bitfield granularity follows field
  bounds**): for `struct {hi:6; span:6; lo:4}`,
  BCC picks read/write granularity per-field:
  - `hi:6` (bits 0-5 of byte 0, fits in 1 byte):
    **byte ops** — `80 66 disp` AND, `80 4e disp`
    OR
  - `span:6` (bits 6-7 byte 0 + bits 0-3 byte 1,
    crosses boundary): **word ops** — `81 66
    disp` AND-word, `81 4e disp` OR-word
  - `lo:4` (bits 4-7 of byte 1, fits in 1 byte):
    byte ops on [bp-1]
  
  So **fields that fit in a byte use byte ops;
  fields that cross use word ops**. Reads follow
  the same rule (mov al vs mov ax).
- `1881` (**enum is int**): enum constants resolve
  at parse time to integer values. `enum Color c
  = GREEN;` compiles to `c = 2` (literal int).
  `c + RED` compiles to `c + 1`. No type
  distinction at codegen.
- `1882` (**typedef is parser alias**): `typedef
  int my_int; typedef my_int alias_int;` is
  purely lexical. Identical codegen to direct
  `int` declarations. Type aliases are resolved
  at parse time; only the underlying type reaches
  codegen.

So the C type system has two layers:
- **Compile-time only** (parser/checker): enum,
  typedef, const qualifier, `(unsigned)` int cast
- **Runtime-affecting**: signedness (changes
  sar/shr/idiv/div choice), pointer types
  (changes far/near, segment fixups), struct
  layout (offsets), bitfield positions

For the Rust reimplementation:
- enum constants → integer values at parse time
- typedef → resolve to base type at parse time
- bitfield granularity: choose byte vs word ops
  based on whether the field crosses a byte
  boundary

## >4B struct return uses hidden ptr + N_SCOPY@ (×2); shift+or = or-with-mem; bitfields work

Fixtures `1877` (6B struct return), `1878` (byte
packing `(hi<<8)|lo`), and `1879` (bitfields in
BCC 2.0) cover remaining struct/bit shapes.

- `1877` (**>4B struct return = hidden ptr +
  N_SCOPY@ × 2**):
  Caller side:
  ```
  ; allocates t (6B) AND temp (6B) — 12 bytes total
  ; push hidden FAR ptr to TEMP (4B)
  call _make_t
  pop / pop                ; cleanup hidden ptr
  ; push FAR ptr to TEMP (source), FAR ptr to t (dest)
  mov cx, 6
  call N_SCOPY@            ; copy temp → t
  ```
  Callee side (`make_t`):
  ```
  ; fill local r
  ; push hidden dest ptr as source-for-copy
  ; push FAR ptr to local r as source
  mov cx, 6
  call N_SCOPY@            ; copy local r → hidden dest
  mov ax, [bp+4]           ; return dest offset in AX
  ```
  So **TWO copies** happen: local r → caller's
  temp (via N_SCOPY@ in callee), then temp →
  caller's named destination (via N_SCOPY@ in
  caller). This wasteful double-copy is a result
  of how BCC chains the temp-buffer protocol.
  
  N_SCOPY@ likely uses pascal/stdcall (callee
  cleans args) since no cleanup visible after the
  in-caller N_SCOPY@ call.
- `1878` (**`(hi << 8) | lo` = shift then or-with-
  mem**):
  ```
  mov ax, [hi]
  mov cl, 8 / shl ax, cl       ; shift
  or ax, [lo]                  ; or-with-memory (0b /r m16)
  ```
  The OR uses `0b 46 fc` (or r16, [bp+disp]) — 3
  bytes. No fusion of shift+or into a single
  operation, but each step uses the optimal
  encoding.
- `1879` (**bitfields WORK in BCC 2.0**): a
  `struct {unsigned a:4; unsigned b:4; unsigned
  c:8;}` is packed into a word at byte granularity:
  - `b.a`: low nibble of byte 0
  - `b.b`: high nibble of byte 0
  - `b.c`: byte 1
  
  Write codegen: clear-mask (AND) + set-mask (OR)
  via **byte ops**:
  ```
  and byte [bp+disp], ~mask    ; 80 66 disp mask  — clear bits
  or  byte [bp+disp], val      ; 80 4e disp val   — set bits
  ```
  Read codegen: load byte → AND with mask →
  shift down if not at LSB → AND again for safety.
  
  Notable: bitfields prefer **byte granularity** when
  the field doesn't cross a byte boundary. `b.c`
  (full byte) uses [bp-1] directly.

For the Rust reimplementation:
- For >4B struct returns: caller allocates temp,
  pushes hidden ptr, then N_SCOPY@'s the temp to
  the named dest. Callee uses N_SCOPY@ to copy
  its local result into the hidden dest.
- `(x << K) | y` lowers to shl then or-with-mem
  (no fusion).
- Bitfields: byte-granular when possible; clear-
  then-set via byte AND/OR with masks.

## 6B struct uses N_SPUSH@; ≤4B struct returns in DX:AX; structs have NO padding

Fixtures `1874` (6-byte struct by value), `1875`
(struct returned by value), and `1876` (struct
with char + int = no padding) complete the
struct ABI picture.

- `1874` (**6-byte struct by value uses
  `N_SPUSH@`**): for structs > 4 bytes, BCC calls
  the helper:
  ```
  lea ax, [s]        ; source offset
  mov dx, ss         ; source segment
  mov cx, 6          ; size in bytes
  call N_SPUSH@      ; helper pushes 6 bytes onto stack
  call _sum_t
  add sp, 6          ; cleanup
  ```
  The helper takes far ptr `(DX:AX)` and count `CX`,
  copies the struct onto the stack via repeated
  push (or some equivalent). Confirms the >4B
  threshold for helper invocation.
- `1875` (**≤4B struct returned in DX:AX**): a
  `struct P {int x; int y;}` (4 bytes) is returned
  via the **DX:AX register pair**:
  - **AX** = first field (low half) — `r.x` = 10
  - **DX** = second field (high half) — `r.y` = 20
  
  Caller stores both back to the destination:
  ```
  call make_p
  mov [p.y], dx      ; high half
  mov [p.x], ax      ; low half
  ```
  This is the classic MS-DOS 8086 small-struct
  return convention. For structs > 4B, a hidden
  caller-allocated buffer ptr is used (not yet
  probed).
- `1876` (**structs have NO padding for
  alignment**): `struct { char c; int n; }` lays
  out as:
  | Field | Offset | Size |
  |-------|--------|------|
  | `c` (char) | 0 | 1 |
  | `n` (int) | **1** | 2 |
  Total: 3 bytes. The int is at an **odd byte
  offset** (1) — accessed via unaligned word
  load/store. 8086 allows this with a cycle
  penalty.
  
  Stack allocation **rounds up** to word boundary
  (4 bytes for a 3-byte struct) — for SP alignment,
  not field alignment.

So the **struct ABI** is:
- Pack: NO padding, tight byte-aligned fields
- Stack-alloc: round up to word (preserve SP word-
  alignment)
- Pass-by-value: ≤4B inline pushes (reverse-mem
  order), >4B uses N_SPUSH@ helper
- Return: ≤4B in DX:AX, >4B via hidden buffer ptr

For the Rust reimplementation:
- Pack structs tight; track field byte offsets.
- Round stack frame to word for SP alignment.
- Emit N_SPUSH@ call for >4B by-value passes;
  inline pushes for ≤4B.
- Emit DX:AX return for ≤4B; hidden ptr for >4B.

## Struct arr-field inline; struct-by-value pushed reverse-mem-order; `o.p->v` 2-step deref

Fixtures `1871` (struct with array field), `1872`
(struct passed by value), and `1873` (chained
struct ptr deref) cover three remaining struct
shapes.

- `1871` (**struct with array field**): `struct {
  int n; int data[3]; }` is laid out **linearly
  with no padding**:
  | Field | Offset |
  |-------|--------|
  | `n` | 0 |
  | `data[0]` | 2 |
  | `data[1]` | 4 |
  | `data[2]` | 6 |
  Total size: 8 bytes. Array-as-struct-field is
  just **inline storage**; constant indices on
  the array resolve at parse time to specific
  flat offsets.
- `1872` (**struct passed by value**): a 4-byte
  struct `P {int x; int y;}` is passed by **field-
  by-field push in REVERSE memory order**:
  ```
  push word [q.y]    ; ff 76 06 — higher offset first
  push word [q.x]    ; ff 76 04 — lower offset last
  call sum_p
  pop / pop          ; 59 59 — 4 bytes cleanup
  ```
  This puts the struct in **memory order** in the
  callee's stack frame ([bp+4]=x, [bp+6]=y).
  Same effect as `memcpy`-ing the source into the
  arg-slot, but with explicit pushes.
  
  For larger structs (>4B), [[batch-XXX-struct-
  push]] (N_SPUSH@ helper) is used. For 4B
  structs, BCC uses inline pushes.
- `1873` (**chained struct ptr deref `o.p->v`**):
  lowers to:
  ```
  mov bx, [o.p]      ; 8b 5e fc — load ptr to BX
  mov ax, [bx]       ; 8b 07    — deref to get .v
  ```
  No fusion or special handling for chained
  derefs. Pointer loaded **once** into BX; then
  one field access. If v had non-zero offset
  (`bx+disp`), the second load would use
  `mov ax, [bx+disp]`.

For the Rust reimplementation:
- Struct layout: linear, no padding (8086 ABI).
  Array fields use sequential element offsets.
- Struct by value (size ≤ 4): inline field-by-
  field push in reverse memory order at call
  site.
- Chained deref `s->f`: emit ptr load to BX, then
  field load via `[bx+disp]`.

## Cross-model uniformity: compact/medium/large share IR-level codegen; differ only at boundaries

Fixtures `1868` (compact static local), `1869`
(medium `&&`), and `1870` (large array via ptr)
verify that **IR-level codegen patterns port
unchanged across models**; differences appear
only at function-call boundaries and pointer
representations.

- `1868` (**compact static local**): the codegen
  is **byte-identical to small-model** for the
  function body — `ff 06 disp16` (inc word [n])
  and `a1 disp16` (mov ax, [n]) via direct DS-
  relative addressing. Compact's "far data" only
  matters when explicit pointers cross segments.
  Function still uses near `ret` (`c3`) since
  compact has near code.
- `1869` (**medium `&&`**): codegen for `if (a &&
  b)` is **byte-identical to small-model** through
  the cmp/jcc chain and bool template. The ONLY
  difference is the function ends with **`5d cb`**
  (pop bp + **retf**) instead of small's `5d c3`
  (pop bp + ret near). Medium has far code, so all
  function returns are far.
- `1870` (**large array via ptr**): pointer ABI
  changes significantly. `int *` is now 4 bytes
  (segment + offset):
  - Callee loads ptr with **`les bx, [bp+6]`** (5
    bytes: `c4 5e 06`) for ES:BX
  - Stores use ES override prefix: **`26 c7 07
    imm16`** (`es: mov word [bx], imm16`, 5 bytes)
  - Each access **reloads** ES:BX (no CSE)
  
  At the call site, BCC uses a **synthetic far
  call**:
  ```
  push ss            ; 16    — push data segment
  lea ax, [x]        ; 8d 46 fc
  push ax            ; 50    — push offset
  push cs            ; 0e    — push CS for retf
  call near _fill    ; e8 disp16 — near call (4B)
  ```
  Total `push cs / call near` = 4 bytes, SHORTER
  than `callf seg:offset` (5 bytes via `9a`). The
  callee's `retf` correctly pops both CS and IP.
  
  This is a Borland-specific optimization: within
  the same segment, fake the far-call protocol
  using cheaper near-call mechanics.

**Cross-model summary**:
| Aspect | Changes per model? |
|--------|---------------------|
| Arithmetic/logic codegen | NO — uniform |
| Register allocation | NO — uniform |
| Boolean/comparison patterns | NO — uniform |
| Encoding policies (imm8-sext, AX-form) | NO — uniform |
| Function `ret` vs `retf` | YES — code-segment-far means retf |
| Pointer width (2 vs 4 bytes) | YES — data-segment-far means 4B |
| Pointer-deref instructions | YES — `mov bx,m / mov [bx]` vs `les bx,m / es: mov [bx]` |
| Synthetic far-call | YES — large/medium use `push cs / call near` |

So **the bulk of the IR-level findings port unchanged**; only the **ABI/pointer boundary** changes per model. This matches the earlier observation from the multi-model fixtures.

For the Rust reimplementation:
- Codegen for non-pointer ops should be model-
  agnostic (shared code path).
- ABI layer: per-model functions for `ret/retf`
  selection, pointer width, push-CS-call-near vs
  callf.

## Pass-by-value writes arg slot only; ptr-arg uses `[si+disp]`; static = global codegen

Fixtures `1865` (modify arg no effect), `1866`
(modify array via ptr), and `1867` (static local
counter) cover param-passing and storage rules.

- `1865` (**arg modify confined to callee**):
  `x = 99` inside callee writes to **`[bp+4]`**
  (the callee's arg-slot copy), NOT the caller's
  storage. After return, caller's x unchanged.
  Standard C pass-by-value semantics — confirms
  the arg is a local copy.
- `1866` (**ptr-arg indexed access**): `a[0] =
  10` and `a[1] = 20` via `int *a` lower to:
  ```
  mov si, [a]               ; load ptr
  mov word [si], 10         ; c7 04 0a 00   (4B, no disp)
  mov word [si+2], 20       ; c7 44 02 14 00 (5B, disp8)
  ```
  ModR/M encodings:
  | Form | ModR/M | Bytes |
  |------|--------|-------|
  | `[si]` | `04` (mod=00, rm=100) | base+imm16 = 4 |
  | `[si+disp8]` | `44` (mod=01, rm=100) | base+disp8+imm16 = 5 |
  | `[si+disp16]` | `84` (mod=10, rm=100) | base+disp16+imm16 = 6 |
  Per-access disp width selection (same rule as for `[bp+disp]`).
- `1867` (**static local = global codegen**):
  `static int n = 0;` inside a function:
  - Storage: `_BSS` (zero-init) at file scope
  - Access: **direct addressing `[disp16]`**, NOT
    stack-relative
  - `inc [n]`: `ff 06 disp16` (4 bytes)
  - `mov ax, [n]`: `a1 disp16` (3 bytes, AX-form
    for memory load)
  
  So `static` only affects **linkage** (internal)
  and **storage duration** (program-lifetime). At
  codegen, statics are identical to globals.

For the Rust reimplementation:
- Function arg modifications write to `[bp+disp]`
  in the callee, never propagate back.
- Pointer-arg array access: per-element `[reg+
  disp]` with disp width chosen per access.
- Static locals: emit as globals in `_BSS` (or
  `_DATA` if non-zero-init), use unique mangled
  name to avoid file-scope name conflicts.

## `(a && b) || c` precedence; `if (fn())` = call+or-ax; `!!x` folds in bool context

Fixtures `1862` (`a && b || c` precedence), `1863`
(`if (fn())`), and `1864` (`!!x`) cover the
remaining boolean idioms.

- `1862` (**mixed `&&`/`||` precedence**): parsed
  as `(a && b) || c` (`&&` binds tighter). The
  codegen converges short-circuit paths via
  forward jumps:
  ```
  cmp [a], 0
  je  L_test_c      ; a=0: && fails, try ||'s rhs
  cmp [b], 0
  jne L_true        ; (a && b) true: skip || rhs
  L_test_c:
  cmp [c], 0
  je  L_false
  L_true: ...
  ```
  Both `&&` operands' "false" paths land at the
  `||`'s rhs test; if the `&&` succeeds, jumps
  directly to true.
- `1863` (**`if (fn())`**): emits `call _yes / or
  ax, ax / je L_false`. The function's return
  value (in AX) is tested via the 2-byte zero-test
  `or ax, ax`. No special handling — same as `if
  (var)` when var is in AX.
- `1864` (**`!!x` folds in bool context**): `if
  (!!x)` lowers to **just** `cmp [x], 0 / je
  L_false / ...` — same as `if (x)`. BCC
  **recognizes the boolean-identity** at parse
  time, eliminating the double-negation sequence.
  
  Only when `!!x` is used **as a value** (e.g.,
  `int b = !!x;`) would the full `neg/sbb/inc /
  neg/sbb/inc` materialization sequence emit.

For the Rust reimplementation:
- Track the **context** (boolean vs value) when
  lowering `!`, `&&`, `||`. In boolean context,
  emit jcc-based short-circuit. In value context,
  materialize into AX via the bool-template
  sequence.
- `!!x` in boolean context: identity, just emit `x`'s
  test directly.

## 3-clause `&&` linearises; `!cmp` folds via inverted jcc; comma yields last operand

Fixtures `1859` (`a && b && c`), `1860` (`!(a <
b)`), and `1861` (`x = (n++, n++, n++)`) cover
remaining boolean/comma edge cases.

- `1859` (**3-clause `&&`**): emits **3 sequential
  cmp+je pairs** with progressively-shorter
  forward jumps to the same false-target (17, 11,
  5 bytes). Each subexpression tested in order;
  any zero skips to false. Standard `&&`
  linearisation extended to N clauses.
- `1860` (**`!(a < b)` folds via inverted jcc**):
  the `!` of a comparison is **simplified at parse
  time** — no boolean materialization needed.
  ```
  cmp ax, b
  jl L_false        ; flipped from jge — was "false-branch for <" = jge; with ! it inverts to jl
  ; true block
  ```
  Effectively: `!(a < b)` lowers to the same code
  as `if (a >= b)` (with the true/false branches
  arranged so the false-branch jcc is `jl`).
  
  General rule: **`!` on any comparison flips the
  false-branch jcc** at parse time, never
  computing the boolean value of the inner cmp.
- `1861` (**comma yields last operand**): `(n++,
  n++, n++)` discards intermediate values; only
  the **last operand's value** is yielded. With
  n=0 initial:
  ```
  xor si, si        ; n = 0
  inc si            ; 1st n++ (value discarded), n=1
  inc si            ; 2nd n++ (value discarded), n=2
  mov [x], si       ; x = 2 (pre-inc value of 3rd n++)
  inc si            ; 3rd n++'s post-inc, n=3
  ```
  Each subexpression emits its side-effect; the
  last one's value is captured into the
  assignment target (handled with the same pre-/
  post-inc capture-order rule).

For the Rust reimplementation:
- `&&` and `||` always emit short-circuit jcc
  chains, never materialise intermediate booleans.
- `!` on a comparison flips the jcc; never compute
  the boolean and negate.
- Comma operator emits each subexpr for side
  effects, captures only the last as value.

## Short-circuit `&&` chains `je/jne`; `||` jumps to true; each operand standalone tested

Fixtures `1856` (`a && b`), `1857` (`a || b`), and
`1858` (`x > 0 && x < 10`) characterise the short-
circuit boolean operators.

- `1856` (**`if (a && b)`**): lowers to
  **sequential cmp+je-on-false**:
  ```
  cmp [a], 0
  je L_false        ; short-circuit: if a == 0, skip
  cmp [b], 0
  je L_false        ; if b == 0, false
  ; fall through to true
  L_true: mov ax, 1 / jmp end
  L_false: xor ax, ax
  ```
  Both operands use the **same false-target**, so
  any zero in the chain skips. Classic short-
  circuit codegen.
- `1857` (**`if (a || b)`**): lowers to **jne-on-
  first then je-on-second**:
  ```
  cmp [a], 0
  jne L_true        ; short-circuit: if a != 0, skip to true
  cmp [b], 0
  je L_false        ; if b == 0, false
  L_true: mov ax, 1 / jmp end
  L_false: xor ax, ax
  ```
  The first operand jumps **forward to true** if
  non-zero; the second uses the standard false-
  branch. So `||` is encoded as "any non-zero
  wins early."
- `1858` (**`x > 0 && x < 10`**): combines `&&`
  with comparisons. Each comparison uses signed
  jcc (since x is signed):
  - `x > 0` → `or si, si / jle L_false` (uses
    zero-test shortcut for `>0`)
  - `x < 10` → `cmp si, 10 / jge L_false`
  Both branches go to a single false-target.

So the **logical operator codegen** is:
| Op | Pattern |
|----|---------|
| `&&` | both operands' inverse-jcc → same false-target |
| `||` | first operand's true-jcc → true-target; second operand's inverse-jcc → false-target |

This matches the inverse-condition pattern used
throughout BCC's branching.

For the Rust reimplementation:
- `&&` emits: lhs-cond / inv-jcc false / rhs-cond /
  inv-jcc false / true-block / jmp end / false-
  block.
- `||` emits: lhs-cond / true-jcc trueblk / rhs-
  cond / inv-jcc false / trueblk / falseblk.

## `if(x & MASK)` = `test [m], imm16`; shift+mask not fused; `(unsigned)int` = no-op

Fixtures `1853` (`if (x & 0x40)` bit test), `1854`
(`(x >> 4) & 0x0F` nibble extraction), and `1855`
(`(unsigned int)int` cast) cover three small but
notable codegen optimisations.

- `1853` (**bit test optimised to `test [m], imm`**):
  `if (x & MASK)` lowers to **`f7 46 disp imm16`**
  (`test word [bp+disp], imm16`, 5 bytes). Sets
  flags from the AND result **without modifying
  memory**. Then `je` branches on ZF. Saves 1 byte
  vs the load + and + jcc sequence.
  
  So `x & MASK` in a **boolean context** (`if`,
  `while`, ternary condition) is recognised and
  optimised to `test`. In an **expression context**
  (used as value), it would use `and ax, MASK`
  instead.
- `1854` (**shift+mask not fused**): `(x >> 4) &
  0x0F` emits both operations sequentially:
  ```
  mov ax, x
  mov cl, 4 / sar ax, cl    ; signed shift since x is int
  and ax, 0x0F              ; AX-form
  ```
  No special fusion. The shift here uses `sar`
  (signed) because x is `int` (signed).
- `1855` (**`(unsigned)int` no-op cast**): casting
  signed to unsigned int emits **no conversion
  code** — just a `mov`. Both are 16-bit; the cast
  is purely a type-system attribute. The behavioral
  difference shows in subsequent ops:
  ```
  signed x; x >> 8  → sar (arithmetic, sign-fill)
  unsigned u; u >> 8 → shr (logical, zero-fill)
  ```
  After the no-op cast, the same bit pattern is
  reinterpreted; later shift uses the appropriate
  opcode for the new type.

For the Rust reimplementation:
- Recognise `x & MASK` in boolean context →
  emit `test [m], imm` instead of `and / jcc`.
- Track signedness through expressions; emit
  sar/shr based on the operand's type at each
  shift point.
- Int↔uint casts emit no code in small/large
  model (both 2-byte ints).

## 5-locals fills full pool; 1-read locals stack; params enregister too

Fixtures `1850` (5 locals), `1851` (3 locals
across call, all 1-read), and `1852` (function
params enregistering) refine the register-
allocation rule.

- `1850` (**5 multi-use locals fill full pool**):
  with 5 locals all needing slots, BCC uses **all
  5 registers**: SI, BX, DI, CX, DX (in
  declaration order, with the pool {SI, DI, DX, BX,
  CX}). No scratch reserved when register pressure
  is high. Confirms the pool's full extent.
  
  Notable: declaration order maps to register
  selection: a→SI, b→BX, c→DI, d→CX, e→DX.
  This may suggest an internal pool order:
  ```
  {SI, BX, DI, CX, DX}
  ```
  (or perhaps {SI, BX, DI, CX, DX} as the BCC
  fill-order — need more probes to confirm).
- `1851` (**1-read locals stay on stack**): 3
  locals each used only once (init + return) all
  stay on stack — even though the function makes a
  call. The threshold for enregistration is firmly
  **≥ 2 reads** in expressions, regardless of
  surrounding context.
- `1852` (**params enregister too**): function
  parameters with ≥ 2 reads also enregister.
  In `do_calc(a, b, c, d)` where a is used twice
  (in `x = a+b` and in `return ...+a`) and d is
  used twice, BCC loads a→SI and d→DI in the
  prologue:
  ```
  mov si, [bp+4]    ; a → SI
  mov di, [bp+10]   ; d → DI
  ```
  Then uses SI/DI throughout the body. Params and
  locals follow the same enregistration rules.

For the Rust reimplementation:
- Count reads (uses in expression contexts) for
  every local AND parameter.
- Variables with ≥ 2 reads enregister using the
  full pool {SI, BX, DI, CX, DX}.
- Prologue includes loads for enregistered params
  (`mov reg, [bp+disp]`).

## `imul` for unsigned mul; `div` for unsigned div (`xor dx,dx`); sentinel loop `cmp [si],0`

Fixtures `1847` (unsigned `* unsigned`), `1848`
(unsigned `/ unsigned`), and `1849` (array sentinel
loop) confirm or refine three encoding rules.

- `1847` (**`imul` for unsigned mul**): BCC uses
  **`imul`** (`f7 /5`) for unsigned multiplication
  too. For 16x16 → 16-bit (low half) results, `imul`
  and `mul` produce identical low halves, so BCC
  uses `imul` uniformly regardless of signedness.
  (Confirms earlier finding from [[batch-437-long-
  mul]].)
- `1848` (**`div` for unsigned div**): unsigned
  division uses **`xor dx, dx / div m16`** (`33 d2 /
  f7 76 disp`), compared to signed `cwd / idiv`:
  | Op | Setup | Divide |
  |----|-------|--------|
  | signed `/` | `cwd` (sign-extend AX to DX:AX) | `idiv` (`f7 /7`) |
  | unsigned `/` | `xor dx, dx` (zero-extend) | `div` (`f7 /6`) |
  
  Both 3 instructions; signedness drives sign-
  extend vs zero-extend of the high half.
- `1849` (**sentinel loop**): `while (*p)` lowers
  to **`cmp word [si], 0`** (`83 /7` with imm8-
  sext 0, 3 bytes) + `jne body`. Tests the
  pointer-dereferenced value against zero via the
  imm8-sext compare.

So the full int-arithmetic encoding now covers:
| Op | Signed | Unsigned |
|----|--------|----------|
| `*` (low 16 bits) | `imul m16` (`f7 /5`) | same |
| `/` 16-bit | `cwd / idiv` | `xor dx,dx / div` |
| `%` 16-bit | (same, read DX) | (same, read DX) |

The `mul` (`f7 /4`) instruction (32-bit result in
DX:AX) is **never used** by BCC for ints — only
the low half via `imul` is wanted.

## `while(--n)` = `dec/jne` (3B); `==`/`!=` inverse jcc; unsigned cmp uses `jae`/`jb`

Fixtures `1844` (`while (--n)`), `1845` (`==`/`!=`
materialization), and `1846` (unsigned `<`) confirm
several optimisation and signedness rules.

- `1844` (**`while (--n)` = dec + jne**): the loop
  test combines the decrement and zero-test into
  **`dec di / jne body`** (3 bytes total: `4f 75
  fc`). The `dec` instruction sets ZF based on
  the result, so the `jne` directly branches on
  it — no separate `cmp` needed. Beautifully
  compact loop test.
- `1845` (**`==` vs `!=` materialization**): both
  use the same boolean template (`cmp / jcc / mov
  ax, 1 / jmp / xor ax, ax`) but with **inverse
  jcc**:
  - `==` true → `jne` (75) for false branch
  - `!=` true → `je` (74) for false branch
  Consistent with the inverse-condition pattern
  applied throughout BCC's codegen.
- `1846` (**unsigned `<` uses `jae`**): for
  `unsigned a < unsigned b`, BCC emits **`jae`**
  (`0x73`, unsigned above-or-equal) for the false
  branch. Critical for correct unsigned semantics:
  `0x8000 < 0x0001` is FALSE unsigned (32768 > 1)
  but TRUE signed (-32768 < 1).

So **signedness drives jcc choice**:
| Op | Signed (jcc-false) | Unsigned (jcc-false) |
|----|--------------------|----------------------|
| `<`  | `jge` (7D) | `jae`/`jnc` (73) |
| `<=` | `jg`  (7F) | `ja`/`jnbe` (77) |
| `>`  | `jle` (7E) | `jbe`/`jna` (76) |
| `>=` | `jl`  (7C) | `jb`/`jc` (72) |
| `==` | `jne` (75) | (same) |
| `!=` | `je`  (74) | (same) |

So FP-cmp uses unsigned-flavour jcc too (per [[batch-
479-fp-cmp]]), matching the FPU's status-word
mapping via `sahf`.

For the Rust reimplementation:
- Combine dec+test loop conditions where the source
  is `while (--var)` or `do {...} while (--var)`.
- Choose jcc based on operand signedness (track
  signed vs unsigned types in the IR).

## `add ax, K`: `inc` for ±1, AX-form for imm8/imm16; `x*2` = `shl ax, 1`

Fixtures `1841` (`x + 50` to AX), `1842` (`x * 2`),
and `1843` (`x + 1` to AX) complete the per-constant
add-encoding picture.

- `1841` (**`x + 50` uses `05` AX-form**): even
  though 50 fits imm8-sext (3 bytes via `83 c0
  32`) which would tie with the AX-form (`05 32
  00`, 3 bytes), BCC consistently picks the
  **`05` AX-form** when the destination is AX.
  No reason to prefer one over the other for byte
  count, but BCC's choice is consistent.
- `1842` (**`x * 2` uses `shl ax, 1`**): the
  pow2-mul shortcut for N=1 is **`shl ax, 1`** (2
  bytes via `d1 e0`). Cheapest possible
  multiplication.
- `1843` (**`x + 1` uses `inc ax`**): 1-byte
  encoding (`40`). The 1-byte register-inc/dec
  opcodes (`40-47` for inc, `48-4F` for dec) are
  preferred over `83 c0 01` (3 bytes) or `05 01
  00` (3 bytes).

**Final add-AX encoding hierarchy** (in order of
preference, smallest first):
| K value | Encoding | Bytes |
|---------|----------|-------|
| +1 | `inc ax` (`40`) | 1 |
| -1 | `dec ax` (`48`) | 1 |
| imm8-sext fits (other) | `05 imm16` (BCC pick) | 3 |
| imm16 only | `05 imm16` | 3 |

For non-AX registers, the choices are:
| K value | Encoding | Bytes |
|---------|----------|-------|
| ±1 | `inc/dec reg` (`40-4F`) | 1 |
| imm8-sext fits | `83 /0 reg imm8` | 3 |
| imm16 only | `81 /0 reg imm16` | 4 |

So **1-byte inc/dec** is always preferred for ±1,
regardless of which register. For larger constants
the AX-form imm16 (3 bytes) wins on AX; non-AX
must use `83`/`81` /N.

This completes the constant-arithmetic encoding
picture for the small-model code generator.

## `sar` same threshold; `add ax, imm16` uses 0x05 AX-form (3B); AND uses 0x25 always

Fixtures `1838` (`sar` by 3), `1839` (`add ax,
1000` AX-form), and `1840` (AND with 127 / 128)
verify the shift-threshold uniformity and the
AX-imm encoding preferences.

- `1838` (**`sar` follows threshold**): signed
  `x >> 3` emits **three `sar ax, 1` unrolled**
  (`d1 f8 d1 f8 d1 f8`). Same threshold rule as
  `shl/shr` — uniform across all shift opcode
  families.
- `1839` (**`add ax, imm16` uses 0x05 AX-form**):
  for `x + 1000` where x is in AX, BCC emits
  **`05 e8 03`** (`add AX, imm16`, 3 bytes) instead
  of the generic `81 c0 imm16` (4 bytes). AX has
  dedicated short-form opcodes for many arithmetic
  ops:
  | Op | AX-form (3B) | Generic (4B) |
  |----|-------------|--------------|
  | `add ax, imm16` | `05 imm16` | `81 c0 imm16` |
  | `sub ax, imm16` | `2d imm16` | `81 e8 imm16` |
  | `cmp ax, imm16` | `3d imm16` | `81 f8 imm16` |
  | `and ax, imm16` | `25 imm16` | `81 e0 imm16` |
  | `or  ax, imm16` | `0d imm16` | `81 c8 imm16` |
  | `xor ax, imm16` | `35 imm16` | `81 f0 imm16` |
  
  BCC consistently prefers AX-forms when applicable.
- `1840` (**AND always uses imm16, never imm8-
  sext**): both `x & 127` (= 0x7F, fits imm8-sext)
  and `x & 128` (= 0x80, doesn't fit imm8-sext)
  emit the **`25 imm16` AX-form** (3 bytes).
  AND/OR/XOR **never** use the `83 /N imm8-sext`
  encoding — the sign-extension of high bit would
  be wrong for bitwise ops. Always full imm16,
  ensuring correct semantics for high-bit-set
  values.

So the encoding-policy table is now fully
characterised:
- **ADD/SUB/CMP**: imm8-sext (`83 /N`) when value
  fits, else imm16 (or AX-form `0X` if dest is AX)
- **AND/OR/XOR**: always imm16 (or AX-form),
  never imm8-sext (would break bit-pattern
  semantics)

For the Rust reimplementation:
- Prefer AX-form opcodes (`05`, `2d`, `3d`, `25`,
  `0d`, `35`) over generic ones when the
  destination is AX.
- Bitwise ops always use full imm16 (or imm8
  with `80 /N` for byte ops).

## Threshold uniform for shl/shr; `x * pow2` recognises and applies same rule

Fixtures `1835` (`x << 3`), `1836` (`x * 32` =
shl by 5), and `1837` (`x * 4` = shl by 2) verify
the unroll-vs-CL-form threshold applies uniformly
across shift opcode families.

- `1835`: `shl ax, 1` × 3 (6 bytes, unrolled at
  N=3) — same shape as `shr` at N=3.
- `1836`: `mov cl, 5 / shl ax, cl` (4 bytes, CL
  form at N=5).
- `1837`: `shl ax, 1` × 2 (4 bytes, unrolled at
  N=2) — `x * 4` correctly recognised as shl by 2.

So the **uniform shift threshold rule applies to**:
- `shl` (left shift / unsigned `<<` / signed `<<` /
  `x * pow2`)
- `shr` (unsigned right shift / unsigned `/ pow2`)
- (presumably `sar` for signed right shift — not
  yet probed)

The `x * pow2` optimization is **recognized at
parse time** and lowered to the same shl encoding
as a direct `x << log2(pow2)`. So fixture 1836
(`x * 32`) is byte-identical to what `x << 5`
would produce.

Updated rule:
- For all shift ops (shl/shr/sar) with constant
  count N:
  - N ≤ 3: unrolled `shift ax, 1` × N
  - N ≥ 4: `mov cl, N / shift ax, cl`
- For `x * 2^N` with N > 0: convert to `x << N`
  at parse time, then apply above.
- For `x / 2^N` (unsigned only) with N > 0:
  convert to `x >> N`, then apply above.

## Shift unroll-vs-CL threshold pinned: N ≤ 3 unrolled, N ≥ 4 CL-form

Fixtures `1832` (N=4), `1833` (N=5), `1834` (N=6)
pin down the exact threshold for the shift-by-N
codegen.

All three use the CL form (`mov cl, N / shr ax,
cl`, 4 bytes total). Combined with earlier
findings:
| N | Encoding | Bytes |
|---|----------|-------|
| 1 | `shr ax, 1` | 2 |
| 2 | `shr ax, 1` × 2 | 4 |
| 3 | `shr ax, 1` × 3 | 6 |
| **4** | **`mov cl, 4 / shr ax, cl`** | 4 |
| 5 | `mov cl, 5 / shr ax, cl` | 4 |
| 6 | `mov cl, 6 / shr ax, cl` | 4 |
| 8 | `mov cl, 8 / shr ax, cl` | 4 |

So the **exact rule**: **N ≤ 3 unrolled, N ≥ 4 CL-form**.

The choice for N=2 is interesting — unrolled (4 bytes)
is tied with CL form (4 bytes) but BCC still unrolls.
Possibly a **performance** consideration on 8086 where
`shr ax, 1` is faster than `shr ax, cl` (which has a
cycle penalty per shift). So the threshold is about
total cycle count, not just byte count.

For N=3: unrolled is 6 bytes (longer) but still chosen
— suggesting BCC prioritises smaller per-shift cycles
over total bytes up through N=3.

For the Rust reimplementation:
- Emit unrolled `shr/shl/sar ax, 1` for N ∈ {1, 2, 3}.
- Emit CL-form `mov cl, N / shift ax, cl` for N ≥ 4.

This refines and **supersedes** the earlier "N ≥ 3 →
CL form" claim from [[batch-469-shift-threshold]] —
the correct threshold is **N ≥ 4**.

## `-x` uses `neg`; `~x` uses `not`; shift-by-3 unrolled (threshold > 3)

Fixtures `1829` (`-x` unary minus), `1830` (`~x`
bitwise not), and `1831` (unsigned `x >> 3`)
refine the unary-op and shift-threshold rules.

- `1829` (**`-x` unary minus**): emits **`neg ax`**
  (`f7 /3`, 2 bytes). Dedicated negation instruction;
  cheaper than `xor ax, ax / sub ax, x`.
- `1830` (**`~x` bitwise NOT**): emits **`not ax`**
  (`f7 /2`, 2 bytes). Dedicated NOT instruction.
  Both `neg` and `not` are 1-byte opcode + 1-byte
  ModR/M = 2 bytes total.
- `1831` (**unsigned `x >> 3` STILL UNROLLED**):
  uses **`shr ax, 1` × 3** (6 bytes), NOT the
  CL-form. This **contradicts the earlier-stated
  N ≥ 3 threshold** from [[batch-469-shift-threshold]].
  
  Refined observation:
  - N=1: 2 bytes (1 shr)
  - N=2: 4 bytes (2 shr)
  - N=3: 6 bytes (3 shr) — still unrolled!
  - N=8: 4 bytes (mov cl, 8 / shr ax, cl) — CL form
  
  So the true threshold is **somewhere between
  N=3 and N=8** (not yet pinned down). Likely
  N ≥ 4 or N ≥ 5. The original "≥3" finding was
  an over-claim from the N=8 example alone.
  
  Empirically: BCC unrolls up through at least
  N=3 even when CL-form would be shorter.

For the Rust reimplementation:
- `neg` and `not` for unary minus and bitwise NOT.
- Shift-by-N threshold for unsigned: use unrolled
  shr for N ≤ 3 (or whatever exact threshold), CL-
  form for higher.
- Need more probes to pin down the exact unroll-vs-
  CL threshold (probably N=4 or N=5).

## Number bases parse-time uniform; for-update comma = 2 ops; char const = int

Fixtures `1826` (octal/hex/dec literals), `1827`
(comma in for-update), and `1828` (char escape
constants) cover three parser-level shapes.

- `1826` (**number bases**): `0x1F`, `077`, `42`
  all resolve to **same imm16 form** at parse
  time. Base prefix (`0x`, `0`, none) is
  consumed by the lexer; the OBJ stores binary
  values uniformly via `c7 46 disp imm16`. Source-
  level base is purely lexical convenience.
- `1827` (**comma in for-update**): `for (i=0,
  j=10; ...; i++, j--)` emits **both update
  statements sequentially** — `inc si / dec di`
  — at the loop's post-update point. No special
  comma handling; just statement sequencing.
  
  Also notable: with 3 multi-use locals (sum, i,
  j), all 3 enregister into DX, SI, DI. DX is
  used here for `sum` (1st declared with multiple
  reads) because the more common SI/DI got the
  loop induction variables.
- `1828` (**char constants**): `'X'`, `'\n'`,
  `'\t'`, `'\\'` are resolved at parse time to
  **int values** (10, 9, 92, 65 respectively).
  Stored via `mov word [m], imm16` since char
  literals have type `int` in C (per K&R / C89
  semantics). Escape sequences and printable chars
  follow the same parse-time resolution.

All three cases reinforce: **the C source-level
representation (base, escape, syntax form) is
purely lexical** — the OBJ contains only the
resolved binary values. BCC's parser does all
the resolution before codegen sees the values.

This matches a common rule across all the
constant-folding evidence: any expression composed
entirely of compile-time-knowable values is
reduced to a single binary constant before being
emitted. Source diversity → single binary form.

## Escapes parse-time resolved; nested ternary linear chain; args eval R-to-L

Fixtures `1823` (escape sequences in string),
`1824` (nested ternary), and `1825` (`sum3(i++,
i++, i++)`) close several remaining shapes.

- `1823` (**string escape sequences**): `"A\nB\t"`
  is resolved at parse time to bytes `41 0a 42 09
  00` in `_DATA`. `\n` → 0x0A, `\t` → 0x09. No
  codegen for escapes — purely a parser-level
  transformation.
- `1824` (**nested ternary**): `x<0 ? -1 : (x==0 ?
  0 : 1)` lowers to a **linear chain of cmp/jcc**
  with materialization into AX:
  ```
  or si, si
  jge L1            ; if NOT < 0
  mov ax, -1
  jmp store
  L1:
  or si, si
  jne L2            ; if NOT == 0
  xor ax, ax        ; 0
  jmp store
  L2:
  mov ax, 1
  store:
  mov [r], ax
  ```
  Nested ternaries don't get fused or specially
  optimized — just sequential evaluation.
- `1825` (**`sum3(i++, i++, i++)` arg order**):
  arguments are **evaluated right-to-left** and
  pushed in that order (matching cdecl R-to-L):
  - First evaluated/pushed: rightmost `i++` (i=5,
    push 5, inc to 6)
  - Second: middle `i++` (i=6, push 6, inc to 7)
  - Third: leftmost `i++` (i=7, push 7, inc to 8)
  
  In callee: `a` (leftmost) = 7 (last pushed),
  `b` = 6, `c` (rightmost) = 5. Sum = 18.
  
  Note: C's order-of-evaluation for fn arg
  expressions is **unspecified** in the spec — BCC
  chose right-to-left, matching the push order.
  Different compilers may differ.

For the Rust reimplementation:
- Resolve escapes at parse time, embed result
  bytes in `_DATA`.
- Lower nested ternary as a flat chain of
  cmp/jcc/jmp with single mov-target.
- Evaluate fn-call args **right-to-left** for
  side-effect-bearing expressions.

## 3D array row-major nesting; arr-of-struct iter = stride-4 shl×2

Fixtures `1820` (3D array constant-indexed), `1821`
(array of struct iteration), and `1822` (loop
init + sum array) confirm multi-dim layout rules.

- `1820` (**3D array `int a[2][2][2]`**): row-major
  layout with offsets computed at parse time for
  constant indices:
  - `a[i][j][k]` offset = `(i*4 + j*2 + k) * 2`
  - `a[0][0][0]` at `[bp-16]`
  - `a[0][1][1]` at `[bp-10]` (offset 6)
  - `a[1][1][1]` at `[bp-2]` (offset 14)
  
  N-dimensional arrays extend the 2D pattern —
  flat linear storage with row-major nesting:
  inner dimensions vary fastest.
- `1821` (**array of struct iteration**): `a[i].x`
  with variable i uses stride-4 (= `sizeof(struct
  P)`) multiplication via **2× `shl bx, 1`**:
  ```
  mov bx, si           ; i
  shl bx, 1            ; *2
  shl bx, 1            ; *2 (= *4)
  lea ax, [a + field]  ; base + field offset
  add bx, ax
  mov ax, [bx]         ; read field
  ```
  Each field access recomputes the address —
  no induction-variable optimization across .x and
  .y in the same iteration.
- `1822` (**loop init + sum array**): two sequential
  loops with the same iteration variable i (SI),
  accumulator (DI). Standard pattern; each
  iteration recomputes the element address via
  `shl + lea + add`.

For the Rust reimplementation:
- N-dimensional array indexing with constant
  indices: precompute the linear offset at parse
  time.
- N-dimensional with variable indices: emit
  multiplication for each dimension (sizeof × index)
  + base.
- Array-of-struct iteration: stride = sizeof(struct);
  if pow2 → shifts; else imul.

## Chain assign reuses AX; pre/post-inc applies to call args; 2D arr row-major

Fixtures `1817` (`a = b = c = 7`), `1818` (`++i` vs
`j++` as call args), and `1819` (2D `a[2][2]` array)
cover three remaining idioms.

- `1817` (**chain assignment**): `a = b = c = 7`
  evaluates right-to-left with AX reused:
  ```
  mov ax, 7
  mov [c], ax
  mov [b], ax       ; AX still holds 7
  mov [a], ax       ; AX still holds 7
  ```
  The value flows up through the chain via the
  register without reloading from memory.
- `1818` (**pre/post-inc as call args**): same
  rule as for assignment context applies to fn
  call args:
  - `identity(++i)` → `inc si / mov ax, si / push`
    (inc first, then capture new value)
  - `identity(j++)` → `mov ax, di / inc di / push`
    (capture old value first, then inc)
- `1819` (**2D array constant indices**): `int
  a[2][2]` is laid out **row-major linear**:
  | C index | Stack offset |
  |---------|--------------|
  | `a[0][0]` | `[bp-8]` |
  | `a[0][1]` | `[bp-6]` |
  | `a[1][0]` | `[bp-4]` |
  | `a[1][1]` | `[bp-2]` |
  
  Compile-time-constant indices resolve to direct
  `[bp+disp]` accesses — equivalent to a flat
  `int a[4]`. No multiply needed for constant
  index pairs.

These three round out the idiom catalog: chain
assignment via AX reuse, pre/post-inc in
expressions, and row-major 2D layout.

## Ptr-walk: swapped cmp + `ja`; missing return falls through; assignment-as-arg

Fixtures `1814` (ptr-walk array via `p < a + 5`),
`1815` (function missing return for some paths),
and `1816` (`sqr(n = 7) + n` assignment in arg)
cover three control-flow / value-flow shapes.

- `1814` (**pointer-walk loop comparison**):
  `for (p = a; p < a + 5; p++)` lowers to:
  ```
  lea ax, [bp+0]    ; ax = &(a+5) = one-past-end
  cmp ax, si        ; flags = ax - si (NOTE: ax is dest!)
  ja body           ; loop while ax > si == p < a+5
  ```
  
  **Notable**: BCC **swaps the cmp operands** so the
  reg-field (= dest) holds the upper bound and the
  r/m field holds the iterator. With the swap,
  unsigned-above (`ja`) correctly continues while
  `p < a+5`. Pointer comparisons use unsigned
  semantics — this works because all SS-addressed
  values within a function frame have ascending
  addresses.
- `1815` (**missing return**): a function declared
  `int` that doesn't return on some path simply
  **falls through to the epilogue with AX
  uninitialized** (whatever value happens to be in
  AX). C makes this UB; BCC emits no safety
  zero-init.
- `1816` (**assignment-in-argument**): `sqr(n = 7)`
  evaluates the assignment as `mov ax, 7; mov [n],
  ax`, then `push ax` (using the assignment's
  value). The expression's value is the RHS, per
  C semantics. After the call, `+ n` reads from
  [n] (which got stored).

For the Rust reimplementation:
- Pointer-comparison loop tests can be encoded as
  `cmp upper_bound, iterator / ja body` — uses
  reg-r/m swap to enable unsigned-above test.
- Don't add safety code for missing return paths —
  emit only what the source specifies; the AX value
  is whatever's there.
- Assignment expressions yield their RHS value;
  emit the store side-effect, then use AX as the
  value.

## Multi-init refs earlier; fn-ptr struct-field call via `ff 56`; `const int` not folded

Fixtures `1811` (multi-init with expressions),
`1812` (fn-ptr struct field), and `1813` (`const
int` folding) cover three more shapes.

- `1811` (**multi-init with cross-references**):
  `int a = 5, b = a + 1, c = b * 2;` works as
  expected — each later initializer references the
  earlier-evaluated variable. Register allocation
  applies per-variable based on read-count:
  - a (used twice): SI
  - b (used twice): DI
  - c (used once, only in return): stack
  
  Variables initialized to expressions still
  qualify for register allocation; init expressions
  are evaluated left-to-right with prior
  declarations visible.
- `1812` (**fn-ptr struct-field call**): `o.f(o.arg)`
  emits **`ff 56 disp`** (`call near [bp+disp]`)
  where disp is the offset of `o.f` within the local
  struct. Same opcode as for local fn-ptr variables
  and fn-ptr parameters. The struct-field-offset is
  baked in at the ModR/M displacement.
- `1813` (**`const int` NOT folded**): `const int
  n = 5; return n * 7;` still allocates a stack
  slot, stores 5, loads it for the multiplication.
  BCC does **not** treat `const` as a hint for
  compile-time folding — the qualifier is purely
  for type-system enforcement (no writes allowed).

For the Rust reimplementation:
- Multi-init: emit each init's code in declaration
  order, with subsequent ones able to read prior
  values.
- Fn-ptr struct field: emit `ff /2 [bp+disp]` with
  appropriate field offset.
- `const` doesn't gate folding — only the parse-
  time constant-folding pass for literal expressions.

So BCC's optimization is **purely syntactic** — it
folds compile-time constants when they appear
directly in expressions (1+2, sizeof(int), etc.) but
not when they hide behind `const` declarations.

## strcpy loop pattern; mul-by-9 uses imul (no shl+add); per-access disp8 vs disp16

Fixtures `1808` (strcpy-like `while (*d++ = *s++)`),
`1809` (`x * 9`), and `1810` (large array with
mixed offsets) reveal three details.

- `1808` (**strcpy-like loop**): the `*d++ = *s++`
  pattern lowers to:
  ```
  mov bx, dx      ; save s
  inc dx          ; s++
  mov al, [bx]    ; load *s
  mov bx, di      ; save d
  inc di          ; d++
  mov [bx], al    ; store *d = *s
  or al, al       ; test for null
  jne body
  ```
  6 instructions for the body. The **post-increment
  pattern** uses BX as a "save the old value"
  register: `mov bx, ptr / inc ptr / [bx]`. The
  assigned value remains in AL for the null-test
  via `or al, al`.
- `1809` (**`x * 9` uses imul**): BCC does NOT
  recognize `shl + add` strength reduction for
  mul-by-non-pow2 constants like 9 (= 8+1). Just
  uses `mov dx, 9 / imul dx` (generic path). Only
  pow2 mul is folded to `shl`; other constants
  always use `imul`.
- `1810` (**per-access disp8 vs disp16**): in a
  function with a large array (160 bytes), different
  element accesses pick disp width independently:
  - `a[0]` at offset `bp-160` uses **disp16** (`c7
    86 60 ff imm16`, 6 bytes per store)
  - `a[70]` at offset `bp-20` uses **disp8** (`c7
    46 ec imm16`, 4 bytes per store)
  
  Each instruction picks the smallest displacement
  that fits its offset. So the **same function can
  mix disp8 and disp16 addressing** based on each
  access's magnitude.

For the Rust reimplementation:
- Implement post-increment pointer pattern with
  BX as the "old value" stash.
- Don't bother with shl+add strength reduction —
  just emit imul for all non-pow2 constants.
- Per-instruction displacement-width selection
  based on offset magnitude.

## 4 enregistered locals: BX joins pool; fn-call doesn't disrupt SI/DI; array-store loop strength

Fixtures `1805` (3 nested loops with 4 locals),
`1806` (fn call in loop), and `1807` (`a[i] = i*i`
+ sum loop) reveal more register-allocation
detail.

- `1805` (**4 locals all enregister**): with sum +
  i + j + k all needing slots, BCC uses **{SI, DI,
  DX, BX}** — extending the pool to 4 registers.
  So the **3-cap rule is a soft default**: when
  more locals qualify than 3, BCC pulls BX (and
  presumably CX) into the pool. Earlier "3-cap"
  observation ([[batch-481-register-allocation]])
  reflected functions where 3 was enough.
  
  Revised: register pool **{SI, DI, DX, BX, CX}**
  with up to 5 slots, but BCC tries to reserve
  BX/CX for scratch when fewer than ~4 locals
  need slots.
- `1806` (**call-crossing locals**): functions
  called inside loops **do not disrupt enregistration**
  of the caller's locals — SI/DI hold sum and i
  throughout the loop, including across the call.
  This works because **SI/DI are callee-save**:
  the callee pushes/pops them in its own prologue/
  epilogue, so the caller's values survive.

  Refines the earlier "call-crossing forces stack"
  rule from [[batch-411-register-allocation]] —
  that may have been about DX (caller-save) not
  SI/DI.
- `1807` (**array-store loop**): `a[i] = i*i`
  emits 7 instructions per iteration:
  ```
  mov bx, si             ; i
  shl bx, 1              ; i*2 (byte offset)
  lea ax, [a]            ; base
  add bx, ax             ; &a[i]
  mov ax, si / imul si   ; i*i
  mov [bx], ax           ; store
  ```
  No induction-variable strength reduction — BCC
  recomputes the address from scratch each
  iteration. A more optimised compiler would
  maintain a pointer and increment it.

So the register pool is more flexible than initially
characterised:
- Default: 3 locals into {SI, DI, DX}, BX/CX scratch.
- Pressure: 4-5 locals into {SI, DI, DX, BX, CX},
  all enregistered, scratch goes to stack.
- Call-crossing for SI/DI: fine (callee-save).
- Call-crossing for DX/BX/CX: may force stack (not
  yet probed precisely).

For the Rust reimplementation:
- Implement 5-slot register pool with use-count
  weighting.
- Track call-crossing per variable; prefer SI/DI
  (callee-save) for call-crossing locals.

## `while(1)` ≡ `for(;;)`; nested loops separate; inner induction may win register

Fixtures `1802` (while(1) + break), `1803` (for(;;)
+ break), and `1804` (nested for loops) cover
remaining loop shapes.

- `1802` (**`while (1)`**) and `1803` (**`for (;;)`**)
  produce **byte-identical code shapes** — both
  emit the standard infinite loop:
  ```
  body:
  inc reg / ...      ; body
  cmp / jle continue
  jmp break_target
  continue:
  jmp body
  ```
  No conditional test before body; just unconditional
  jump back to body from continue point.
- `1804` (**nested loops**): standard structure
  with no fusion or special handling. Outer
  iteration `i` ends up on **stack** while inner
  iteration `j` got **DI** (register). With:
  - sum (1st declared): SI
  - i (2nd, outer-loop only): stack
  - j (3rd, inner-loop): DI
  
  Despite declaration order suggesting i should get
  the register slot, **the inner-loop induction
  variable won** — possibly due to loop-depth
  weighting in BCC's register allocator. Hot inner-
  loop variables get priority over outer overhead
  variables when register slots are limited.

This refines the register-allocation rule from
[[batch-482-register-allocation]]: among equally-
qualified candidates, **loop-depth weighting** can
override pure declaration order. Variables accessed
in deeper loops are weighted higher.

So the final register-selection priority:
1. `register` keyword (mandatory).
2. Highest read-count in expressions.
3. Loop-depth-weighted read count (inner loops
   count more).
4. Earliest declaration (tiebreak).

For the Rust reimplementation:
- Compute per-variable "weighted use count" =
  `sum over all reads of: 1 * (loop_depth + 1)`
  (or similar weighting).
- Select up to 3 highest-weighted into SI, DI,
  DX.

## Arrays ALWAYS use N_SCOPY@; inline path is struct-only

Fixtures `1799` (`int a[2] = {5,10}` 4-byte
array), `1800` (`int a[1] = {7}` 2-byte array),
and `1801` (`char a[2] = {'A','B'}` 2-byte char
array) reveal that arrays never use the inline
mov path — they always go through `N_SCOPY@`,
even when the size would qualify a struct for the
inline shortcut.

- `1799` (4-byte int array): N_SCOPY@ with 4-byte
  template. Same size as 2-int struct (which uses
  2 inline movs) — but the array version uses the
  helper.
- `1800` (2-byte int array): N_SCOPY@ with 2-byte
  template. Same size as 1-int struct (1 inline
  mov) — array uses helper.
- `1801` (2-byte char array): N_SCOPY@. Even 2
  bytes of char data goes through the helper.

So the **revised aggregate-init rule**:
| Type | Size | Init mechanism |
|------|------|----------------|
| Struct, 2 bytes (1 word) | 1 word | 1 inline mov via AX |
| Struct, 4 bytes (2 words, even-byte) | 2 words | 2 inline movs via AX, DX |
| Struct, 3 bytes / odd-byte | N/A | N_SCOPY@ |
| Struct, > 4 bytes | any | N_SCOPY@ |
| **Array of any element / any size** | any | **N_SCOPY@** |

So the rule simplifies to: **inline path is reserved
for word-aligned 1-2 word structs**; everything else
(odd-byte structs, arrays of any shape) uses N_SCOPY@.

This is consistent with the **type-based homogeneity**
in BCC's parser — arrays as a type-class always go
through the bulk-copy path; structs get the
optimization when shape allows.

For the Rust reimplementation:
- If aggregate-type is struct AND size ∈ {2, 4}
  AND fields are word-aligned: emit inline movs.
- Otherwise: emit N_SCOPY@ with template in `_DATA`
  + dest pointer on stack.

## 2-byte struct = 1 mov; long-field struct = 2 movs; odd-byte struct = N_SCOPY@

Fixtures `1796` (1-int struct = 2B), `1797` (1-
long struct = 4B), and `1798` (int+char struct =
3B) refine the aggregate-init boundary to the
byte level.

- `1796` (**1-field 2-byte struct**): `struct { int
  x; } = {42}` uses a **single mov** through AX —
  `mov ax, [template] / mov [p_x], ax`. No N_SCOPY@.
- `1797` (**1-field 4-byte struct (long)**): `struct
  { long a; } = {100L}` uses **2 movs** (one per
  half via AX and DX):
  ```
  mov ax, [template + 2]    ; high
  mov dx, [template + 0]    ; low
  mov [p_high], ax
  mov [p_low], dx
  ```
  So the inline path treats the long as 2 word-
  halves, same shape as a 2-int struct (`1795`).
- `1798` (**3-byte struct (int+char) uses
  N_SCOPY@**): even though only 3 bytes, BCC can't
  inline because the int+char layout doesn't fit
  into word-aligned mov pairs. Template in `_DATA`
  is packed (3 bytes: `64 00 41`); local slot is
  padded to 4 bytes; N_SCOPY@ copies exactly 3
  bytes (the padding byte is not initialized).

Refined boundary by **structural shape**:
| Size | Layout | Init mechanism |
|------|--------|----------------|
| 2 bytes | 1 word | 1 mov via AX |
| 4 bytes | 2 words (int+int OR long) | 2 movs via AX/DX |
| 3 bytes | int+char (odd) | N_SCOPY@ |
| > 4 bytes | any | N_SCOPY@ |

So the rule is **"can the struct be loaded/stored
as 1 or 2 word-aligned chunks?"**. Word-aligned
2-byte and 4-byte structs inline; odd-byte
structs always go through the byte-precise
N_SCOPY@.

Local slot is **padded to even size** for word
alignment even though template is packed.

For the Rust reimplementation:
- 1 word struct → 1 inline mov from template
- 2 word struct (including long) → 2 inline movs
- Mixed/odd-byte struct → N_SCOPY@ with packed
  template + over-allocated slot

## Local aggregate-init boundary: ≤4B struct = 2 movs; >4B struct or any array = N_SCOPY@

Fixtures `1793` (`int a[5] = {0}` all zeros),
`1794` (3-field struct init), and `1795` (2-field
struct init) refine the aggregate-init boundary
further.

- `1793` (**all-zeros array still uses N_SCOPY@**):
  even `int a[5] = {0}` (all 10 bytes zero) emits
  the **N_SCOPY@ call** with a 10-byte zero
  template in `_DATA`. BCC doesn't optimize this
  to `rep stosw` or a zero-fill loop. The zero
  template wastes 10 bytes in the OBJ but the
  codegen path is uniform.
- `1794` (**3-field struct → N_SCOPY@**): `struct
  P { int x, y, z; } p = {10, 20, 30};` (6 bytes)
  uses N_SCOPY@ from a template in `_DATA`. Same
  as arrays of any size.
- `1795` (**2-field struct → inline movs**): a
  4-byte struct `{int x, y;} = {10, 20}` uses
  **inline initialization** via two `mov`
  instructions reading from the template:
  ```
  mov ax, [_template + 2]    ; y value
  mov dx, [_template + 0]    ; x value
  mov [p_y_slot], ax
  mov [p_x_slot], dx
  ```
  No N_SCOPY@ call — just two direct-memory loads
  and stores. Saves the helper call overhead.

So the **aggregate-init boundary** is:
| Aggregate | Size | Init mechanism |
|-----------|------|----------------|
| struct (≤ 4 bytes) | 1-2 words | Inline 1-2 movs from template |
| struct (> 4 bytes) | 3+ words | N_SCOPY@ from template |
| array (any size) | always | N_SCOPY@ from template |

The 4-byte boundary matches the DX:AX return-pair
size — same as the struct-return ABI ([[batch-455-
return-abi]]). Borland's design consistently uses
this width as the "register pair fits" cutoff.

For the Rust reimplementation:
- Emit inline mov-from-template only for ≤4B
  structs.
- All arrays (even tiny ones) go through N_SCOPY@.
- Zero-init aggregates aren't specially optimized
  — same N_SCOPY@ path.

## Static local non-zero init → `_DATA`; local arr `{...}` init always uses `N_SCOPY@`

Fixtures `1790` (static local int with init = 5),
`1791` (`int a[3] = {1,2,3}`), and `1792` (`int
a[6] = {1..6}`) refine the local-aggregate-init
codegen rules.

- `1790` (**static local with non-zero init →
  `_DATA`**): `static int n = 5;` places n in
  **`_DATA`** with the initial value baked into
  LEDATA (`05 00`). The function accesses it
  via `inc word [_n]` (`ff 06 disp`) and `mov ax,
  [_n]` (`a1 disp`) — FIXUPP-resolved direct memory.
  
  Contrast with `static int n = 0;` (or no init)
  which would go to `_BSS` (no LEDATA needed since
  loader zeros BSS at startup).
- `1791` (**`int a[3] = {1,2,3}` uses `N_SCOPY@`**):
  An aggregate initializer for a local array
  **always uses `N_SCOPY@`** to copy from a
  template in `_DATA` — NOT inline `mov [m], imm`
  stores. The 3-int template (6 bytes) is in DATA;
  N_SCOPY@ copies it onto the stack.
- `1792` (**same for `int a[6]`**): identical
  shape, just 12 bytes of template.

So the **local-aggregate-init rule** is:
| Pattern | Codegen |
|---------|---------|
| `int a[3];` then `a[0]=1; a[1]=2; ...` | Inline `mov [bp+disp], imm` per element |
| `int a[3] = {1,2,3};` | Template in `_DATA` + `N_SCOPY@` |
| `int a[N];` (no init) | `sub sp, N*2` only, no init code |

So **aggregate-init in declaration always uses
N_SCOPY@**, regardless of size — even for 3-ints
which would be only 12 bytes of inline stores. The
template+copy approach is more compact: 6 bytes
of template in `_DATA` + ~15 bytes of call setup,
vs 18 bytes (3 × `c7 46 disp imm16`).

For larger arrays, N_SCOPY@ becomes increasingly
efficient. BCC's design choice: always template+
copy for aggregate-init syntax; programmers who
want inline stores write separate assignments.

For the Rust reimplementation:
- When emitting local aggregate-init from `{...}`:
  emit template to `_DATA`, then call `N_SCOPY@`
  with appropriate count and src/dest far pointers.

## Fn-ptr arg call via `[bp+4]`; uninit local has no init code; uninit globals → BSS

Fixtures `1787` (fn taking fn-ptr arg), `1788`
(uninit local int), and `1789` (uninit globals)
clarify the global-storage and uninit semantics.

- `1787` (**fn-ptr as parameter**): the callee
  invokes the fn ptr via **`ff 56 04`** = `call
  near [bp+4]`. Same `ff /2` indirect call as for
  local fn ptrs, just with `[bp+disp]` addressing
  for the parameter slot. Caller passes the ptr via
  `mov ax, &fn / push ax`. No special protocol for
  fn-ptr args.
- `1788` (**uninitialized local int**): `int x;`
  (no init) allocates the stack slot via `dec sp`
  but emits **no init store**. The slot contains
  garbage. Distinct from `int x = 0;` which would
  emit `mov word [m], 0`. Reading before assignment
  is UB; programmers must write first.
- `1789` (**uninitialized globals → BSS**): `int g;
  int h;` (no init at file scope) reserves
  **2 bytes each in `_BSS`** segment with PUBDEFs:
  - BSS SEGDEF size = 4 bytes (2 ints).
  - Both PUBDEFs emitted (external linkage by
    default).
  - OS loader zero-initializes BSS at startup —
    no space in OBJ for the zero values.
  
  Initialized globals would go to **`_DATA`** with
  their values baked into LEDATA records.

Combining with `1786` and earlier rules, the global
storage decision matrix is:
| Source | Storage | OBJ | PUBDEF |
|--------|---------|-----|--------|
| `int g = 0;` | `_DATA` | LEDATA with 00 00 | yes |
| `int g = 1;` | `_DATA` | LEDATA with 01 00 | yes |
| `int g;` (no init) | `_BSS` | size in SEGDEF | yes |
| `static int s = 1;` | `_DATA` | LEDATA | no |
| `static int s;` | `_BSS` | size in SEGDEF | no |

Note: `int g = 0;` could theoretically go to BSS
(since it's zero), but BCC keeps it in DATA. Same
for `static int s = 0;`.

For the Rust reimplementation:
- Track per-global: has-init flag → `_DATA` vs
  `_BSS`; linkage flag → PUBDEF or not.
- Uninit locals just `sub sp` for the slot; no
  init store.
- Fn-ptr params accessed at `[bp+disp]` and called
  via `ff /2`.

## `if(long)` = OR-halves; partial array init zero-fills `_DATA`; `static` global no PUBDEF

Fixtures `1784` (`if(long)` truthiness), `1785`
(partial array initializer), and `1786` (static
vs extern globals) cover three remaining shapes.

- `1784` (**`if (long)` truthiness**): uses the
  **OR-halves trick** identical to `long == 0`:
  ```
  mov ax, low
  or ax, high      ; ZF set iff both halves zero
  je L_false       ; or jne for falsy
  ```
  3 instructions test all 32 bits. Same shortcut
  from [[batch-473-long-cmp-zero]] applies for
  if-truthiness.
- `1785` (**partial array init zero-fills**): an
  initializer like `int a[5] = {1, 2}` zero-fills
  the remaining 3 elements. BCC places the **whole
  array in `_DATA`** as 10 bytes `01 00 02 00 00 00
  00 00 00 00`. Could theoretically split into
  `_DATA` (initialized prefix) + `_BSS` (zero
  suffix) for large arrays, but BCC keeps it simple.
- `1786` (**static vs extern globals**): both go
  to **`_DATA`** packed sequentially, but:
  - `static int s = 10`: no PUBDEF (internal
    linkage). Same-TU references resolve at codegen.
  - `int g = 20`: PUBDEF emitted (external
    linkage). Visible to linker.
  Both accessed via direct-memory `a1 disp` / `03 06
  disp` with FIXUPP. Confirms static-globals follow
  the same emit-but-don't-export rule as static
  functions ([[batch-463-static-fn]]).

So `static` storage class is consistent across:
- Functions: emit to `_TEXT`, no PUBDEF.
- Initialized globals: emit to `_DATA`, no PUBDEF.
- Uninitialized globals (presumed): reserve in
  `_BSS`, no PUBDEF.
- Locals: BSS for static-local (no PUBDEF since
  they're not externally visible anyway).

For the Rust reimplementation:
- Implement OR-halves zero-test for long
  if-truthiness (same code as `long == 0`).
- Zero-fill `_DATA` for partial aggregate init —
  no need to split into BSS.
- Track linkage flag per symbol: emit PUBDEF only
  for default-extern (non-static) symbols.

## Long mod-pow2 = N_LMOD@ (no AND); ulong `>>1` = `shr/rcr`; `long * 1L` folds to identity

Fixtures `1781` (signed long mod by pow2), `1782`
(unsigned long `>>1`), and `1783` (long * 1L)
extend the long-arithmetic folding picture.

- `1781` (**signed long mod by pow2 still uses
  helper**): `a % 4L` (signed long) lowers to a
  full **`N_LMOD@` call**, NOT inlined as
  `and ax, 3 / and dx, 0`. Same reasoning as
  signed int mod by pow2 ([[batch-468-signed-mod-
  pow2]]) — AND-mask gives wrong (unsigned) result
  for negative dividends. The 4-byte divisor 4L is
  pushed onto the stack.
- `1782` (**unsigned long `>>1` inline**): mirrors
  the signed long `>>1` from [[batch-473-long-shr-
  1]] but with `shr` (logical) instead of `sar`
  (arithmetic) for the high half:
  ```
  shr high, 1     ; d1 e8 — zero-fill high bit
  rcr low, 1      ; d1 da — rotate CF into top of low
  ```
  The `rcr` is the same for both signed and unsigned
  (it doesn't preserve sign, just chains carry).
  Only the high-half opcode differs (`sar`/`shr`).
- `1783` (**`long * 1L` folds to identity**):
  multiplication by 1 (long) is recognised at parse
  time — **no `N_LXMUL@` call**, just a `mov`
  copy of a to r. Same as `int * 1` folding.
  Extends the constant-folding catalogue to longs:
  - `long * 1L` → identity (mov copy)
  - `long + 0L` → identity (presumed)
  - `long << 0` → identity (presumed)
  - `long * 0L` → 0L (presumed)

For the Rust reimplementation:
- Mirror the AND-mask vs idiv decision for
  signed/unsigned mod at all widths.
- Long shifts by 1 inline as `shift-high / rcr-low`
  with the appropriate signedness opcode for the
  high half.
- Apply identity-folding to long ops at parse time.

## Ptr arith stride matches type; `(int)ptr` = no-op cast; K&R `()` accepted

Fixtures `1778` (char* vs int* pointer arithmetic),
`1779` (cast pointer to int), and `1780` (K&R empty
parens) cover three remaining language shapes.

- `1778` (**pointer arithmetic stride**): `p++`
  emits a stride based on the pointee size:
  | Pointee | `p++` instructions |
  |---------|--------------------|
  | char (1) | `inc reg` (1 byte) |
  | int  (2) | `inc reg ; inc reg` (2 bytes) |
  | long (4) | `add reg, 4` (3 bytes; not yet probed) |
  | double (8) | `add reg, 8` |
  
  Also reconfirms: char write uses `c6 /N + imm8`
  (`mov byte [si], 7`); int write uses `c7 /N +
  imm16` (`mov word [si], 20`).
- `1779` (**`(int)ptr` no-op cast**): the cast is
  **purely a type-system fiction** in small model
  — both `int` and `int *` are 2 bytes, so the
  pointer's bits are reinterpreted as int with no
  emission. The expression `v - v` does NOT fold to
  0 — BCC emits `sub ax, si` (2 bytes) as if it
  matters. So **same-register self-subtract is
  not optimised** at this level.
- `1780` (**K&R `()` syntax**): `int get42()` and
  `int main()` (empty parens, no `void`) compile
  to **byte-identical code** as `int get42(void)`
  and `int main(void)`. Permissive K&R legacy —
  BCC doesn't enforce argument checking when the
  prototype is omitted.

For the Rust reimplementation:
- Pointer arithmetic emits stride-appropriate
  inc/inc... or add reg, N based on `sizeof(*p)`.
- `(int)ptr` and `(ptr)int` casts in small model
  emit no conversion code — just register reuse.
- K&R `int f()` decl should be accepted, treated
  as "any args" matching ANSI default.

## Fn returns ptr via AX; 3-call cascade direct; `long + int_const` = add/adc

Fixtures `1775` (function returns int *), `1776`
(3-level call cascade), and `1777` (mixed
`(long)i * l + 7`) cover three remaining shapes.

- `1775` (**fn returning pointer**): emits **`mov
  ax, FIXUPP_to_symbol`** (3 bytes) for `return
  &g;`. Standard near pointer return in AX. The
  link-time FIXUPP resolves to the actual data
  segment offset.
- `1776` (**3-level call cascade**): `sqr(dbl(inc
  (2)))` evaluates innermost-first with sequential
  push/call/pop pairs. Each call's return AX is
  directly reused via `push ax` as the next
  outer call's arg — no intermediate spilling:
  ```
  mov ax, 2 / push ax
  call _inc / pop cx
  push ax            ; inc's result
  call _dbl / pop cx
  push ax            ; dbl(inc(2))
  call _sqr / pop cx
  ```
- `1777` (**`(long)i * l + 7`**): mixed long
  arithmetic with int constant tail.
  - Promote int i to long via `cwd`.
  - **Register shuffle via stack** to put (long)i
    in CX:BX and l in DX:AX (the N_LXMUL@ ABI):
    ```
    push ax        ; low of (long)i
    push dx        ; high of (long)i
    mov dx, l_high
    mov ax, l_low
    pop cx         ; high → CX
    pop bx         ; low → BX
    call N_LXMUL@
    ```
  - **Add int constant `+ 7` to long**: inlined as
    `add ax, 7 / adc dx, 0` (5 bytes). The 7 is
    treated as a long with high=0; `adc dx, 0`
    propagates the carry from the low-half add
    into the high half.

So `long + int_const` is **always inlined** (no
helper needed since carry propagation is just 2
instructions). Same for `long - int_const` (would
use `sub ax, K / sbb dx, 0`).

The register-shuffle pattern in 1777 is notable
because BCC uses the stack as a temporary swap
space when the two long operands need to be in
specific register pairs (CX:BX vs DX:AX) and
both come from memory. Push low/high, load the
other, pop in reverse — effectively swapping
without a 3rd register.

## Huge ptr family: `N_PADA@`/`N_PSBA@`/`N_PSBP@`/`N_PCMP@`

Fixtures `1772` (huge ptr ==), `1773` (huge ptr1 -
huge ptr2), and `1774` (huge ptr--) complete the
huge-pointer helper family.

- `1772` (**huge `==` uses `N_PCMP@`**): compares
  two huge pointers via the helper. ABI:
  - DX:AX = first huge ptr (seg:off)
  - CX:BX = second huge ptr
  - Returns flags (ZF set if equal)
  
  Note: helper compares the **normalized physical
  addresses**, not just bit-for-bit seg:off — so
  `0000:0010 == 0001:0000` correctly (both refer to
  same physical 0x10).
- `1773` (**huge ptr difference**): `p2 - p1`
  (element count) uses **two helpers in sequence**:
  1. **`N_PSBP@`** computes byte-difference
     between the two normalized huge pointers
     (returns a long).
  2. **`N_LDIV@`** divides that long by
     `sizeof(element)` (= 2 for int) to get the
     element count.
  
  So C's pointer-subtraction semantics for huge
  pointers needs two helper calls.
- `1774` (**huge `p--` uses `N_PSBA@`**): the
  Subtract-And-assign counterpart to `N_PADA@`
  (Add-And-assign from 1771). Same ABI pattern:
  - DX:AX = far ptr to the pointer variable
  - CX:BX = decrement magnitude (32-bit)
  - Modifies the pointer in place with proper
    normalization.

Complete huge-pointer helper family:
| Helper | C source | ABI |
|--------|----------|-----|
| `N_PADA@` | `p++` / `p += n` | dx:ax=&p, cx:bx=n |
| `N_PSBA@` | `p--` / `p -= n` | dx:ax=&p, cx:bx=n |
| `N_PSBP@` | `p1 - p2` (bytes) | dx:ax=p2, cx:bx=p1 → long |
| `N_PCMP@` | `p1 == p2` etc. | dx:ax=p1, cx:bx=p2 → flags |
| (presumed) `N_PADP@` | `p + n` (value) | not yet probed |
| (presumed) `N_PSBC@` | comparison forms | not yet probed |

Borland's naming pattern: `N_P` = pointer helper, 3-letter
suffix indicates operation:
- `ADA` = ADd-Assign
- `SBA` = SuBtract-Assign
- `SBP` = SuBtract-Pointer (returns long)
- `CMP` = CoMPare
- `@` = external symbol marker

For the Rust reimplementation:
- Track `huge` qualifier on pointer types separately
  from `far`.
- Emit the appropriate `N_P*@` helper based on
  operator: `+=`/`++` → ADA, `-=`/`--` → SBA, `==`/
  `!=` → CMP, `-` (ptr-ptr) → SBP + LDIV.

## Tiny = same as small (model byte only); Huge adds DS save/restore + `N_PADA@`

Fixtures `1769` (`-mt` tiny), `1770` (`-mh` huge),
and `1771` (huge ptr arith in -ms) complete the
**full 6-model coverage**.

- `1769` (**`-mt` tiny**): produces **byte-
  identical code to `-ms`** at the per-function
  level. Same `_TEXT`/`_DATA` segments, same call/
  ret encoding. Differs only in the **COMENT model
  marker byte** (`08` for tiny vs `09` for small)
  which tells the linker to merge all segments
  into one (max 64K total program).
- `1770` (**`-mh` huge**): introduces **per-module
  DS swap** in every function:
  ```
  _func:
  push bp
  mov bp, sp
  push ds                   ; 1e — save caller's DS
  mov ax, MOD_DGROUP        ; b8 imm16 (FIXUPP)
  mov ds, ax                ; 8e d8 — switch to our DGROUP
  ; body
  pop ds                    ; 1f — restore caller's DS
  retf                      ; cb
  ```
  Plus new segment name `HELLO_DATA` (the module's
  own data segment, parallel to `HELLO_TEXT` for
  code). So huge model adds **~6 bytes of DS save/
  load** to every function prologue and **1 byte
  of DS restore** to every epilogue. Each TU
  effectively has its own data segment.
- `1771` (**huge ptr arith uses `N_PADA@`**):
  with `int huge *p`, doing `p++` calls **`N_PADA@`**
  (huge pointer add). The helper:
  - **DX:AX** = far ptr to operand pointer
  - **CX:BX** = 32-bit increment value
  - Adjusts the segment:offset so the result is
    properly **normalized** across 64K boundaries

  This is the **key distinction** between far and
  huge pointers:
  - `far *` `p++`: increments offset only (1651) —
    breaks at 64K boundaries
  - `huge *` `p++`: uses `N_PADA@` to renormalize
    the segment:offset — properly handles >64K data

Full 6-model coverage matrix:
| Model | Code | Data | Seg name | retX | call site | Special |
|-------|------|------|----------|------|-----------|---------|
| `-mt` tiny | near | near | `_TEXT` | ret | call near | merged at link |
| `-ms` small | near | near | `_TEXT` | ret | call near | (default) |
| `-mc` compact | near | far  | `_TEXT` | ret | call near | & = 4B fp |
| `-mm` medium | far  | near | `HELLO_TEXT` | retf | push cs/call | |
| `-ml` large | far  | far  | `HELLO_TEXT` | retf | push cs/call | & = 4B fp |
| `-mh` huge | far  | far  | `HELLO_TEXT`+`HELLO_DATA` | retf | push cs/call | DS swap, N_PADA@ |

And **huge-pointer helper**:
| Helper | Purpose | ABI |
|--------|---------|-----|
| `N_PADA@` | huge ptr addition | dx:ax = ptr, cx:bx = inc; normalises |
| (presumably `N_PCMP@`, `N_PSBA@`, etc. for cmp/sub) | not yet probed | |

This completes the cross-model story. For the Rust
reimplementation, all 6 models are characterised
and the model-conditional emission is well-bounded.

## Medium = far-code, near-data; Compact = near-code, far-data; segment capture varies

Fixtures `1766` (medium `-mm` fn call), `1767`
(compact `-mc` fn call), and `1768` (compact &global)
extend cross-model coverage to all 4 standard
memory models.

- `1766` (**`-mm` medium**): far code, near data.
  - Segment name: **`HELLO_TEXT`** (like -ml).
  - Function ABI: `retf`, args at `[bp+6]`, `push
    cs ; call near` at sites. **Identical to -ml
    for code-only ops**.
  - Data ABI: would be near (DS-relative) since
    "near data" is the model — but this fixture
    doesn't touch globals.
- `1767` (**`-mc` compact**): near code, far data.
  - Segment name: **`_TEXT`** (like -ms).
  - Function ABI: `ret` (near), args at `[bp+4]`,
    `call near` at sites. **Identical to -ms for
    code-only ops**.
  - Data ABI: would be far when address-taken.
- `1768` (**`-mc` `&global`**): confirms compact's
  far-data nature. `int *p = &g` produces a **4-
  byte far pointer**:
  ```
  mov [bp-2], ds        ; 8c 5e fe — capture DS (not SS!)
  mov [bp-4], &g        ; FIXUPP'd offset
  les bx, [p]
  mov es:[bx], 42        ; 26 prefix
  ```
  Notable: the segment capture is **`mov [m], ds`**
  (`8c /3`) since g lives in DS, not `mov [m], ss`
  (`8c /2`) like for stack-local addresses. Function
  itself uses near `5d c3` ret.

Final memory-model matrix:
| Model | Code | Data | Seg name | retX | call site |
|-------|------|------|----------|------|-----------|
| `-ms` small | near | near | `_TEXT` | ret | call near |
| `-mc` compact | near | far  | `_TEXT` | ret | call near |
| `-mm` medium | far  | near | `HELLO_TEXT` | retf | push cs / call near |
| `-ml` large | far  | far  | `HELLO_TEXT` | retf | push cs / call near |

And **segment-register capture by storage class**:
| Storage | Segment | Capture opcode |
|---------|---------|----------------|
| stack (local) | SS | `8c /2` |
| global / static (DS) | DS | `8c /3` |
| code (CS) | CS | `8c /1` (rare) |
| ES, GS, FS | — | `8c /0`, etc. |

For the Rust reimplementation:
- `code_model: Near|Far` → controls call/ret + push cs + seg name + arg offset base.
- `data_model: Near|Far` → controls pointer width and seg capture for & operator.
- These are **independent** parameters — small=NN, compact=NF, medium=FN, large=FF.

## `register` keyword forces enreg; use-count breaks ties; `&x` forces stack

Fixtures `1763` (register keyword), `1764` (varying
use counts), and `1765` (address-taken var)
clarify the register allocation policy further.

- `1763` (**`register` keyword**): `register int
  n;` explicitly enregisters n into SI even though
  there are 5 locals total. The `register` hint
  takes priority over other selection criteria,
  guaranteeing the variable gets a register slot.
- `1764` (**3 locals, all enregister**): with
  exactly 3 multi-use locals (no spills), each
  gets its own register: rare→DI, often→SI,
  seldom→DX. So the register pool ordering for
  3 simultaneous locals is **{DI, SI, DX}** when
  no register-keyword hints are present.

  Combined with `1760`'s observation (`a→SI,
  c→DI, e→DX` when a, c, e win the 3-slot lottery
  against b, d, f):
  - When all qualifying locals fit, the
    declaration-order maps to a specific register
    sequence.
  - When more qualify than fit, **use-count breaks
    the tie**: locals with higher read-count win
    register slots over locals with lower counts.
  - In 1760, a/c/e have 3 uses each, b/d/f have 2
    uses each — so the 3-use group wins all 3
    slots regardless of declaration order parity.
  - In 1764, all 3 qualify; assignment is DI/SI/DX
    by declaration order (though precise mapping
    may depend on register pressure analysis).

- `1765` (**`&x` forces stack**): when `&x` is
  taken, x **stays on the stack** regardless of
  use count — needed so the address is addressable.
  `*p = *p + 3` becomes:
  ```
  mov ax, [si]       ; load *p
  add ax, 3
  mov [si], ax       ; store *p
  ```
  And `x = x + 1` becomes a memory-RMW:
  ```
  mov ax, [bp-2]
  inc ax
  mov [bp-2], ax
  ```
  The pointer p, however, enregisters into SI
  (it's an automatic local without taken address).

Updated register-allocation rule (final):
1. **Mandatory enregistration** (override pool
   limits): `register` keyword.
2. **Mandatory stack**: `&var` taken, `volatile`
   qualifier.
3. **Candidates**: remaining locals with read-count
   ≥ 2 in expressions (init/single-write doesn't
   count).
4. **Selection**: up to 3 candidates win register
   slots. When more qualify than slots:
   - Higher use-count breaks ties first.
   - Among equal use counts, declaration order
     (earliest wins).
5. **Pool**: {SI, DI, DX}. BX/CX reserved for
   scratch.

## 6 locals → only 3 enregister (SI/DI/DX); array param decays; mutual recursion uses relative

Fixtures `1760` (6 multi-use locals), `1761` (array
parameter), and `1762` (mutual recursion) clarify
the register allocation cap and call mechanics.

- `1760` (**register allocation cap = 3 locals**):
  with 6 locals all used 2+ times in source, BCC
  enregisters **only the 1st, 3rd, 5th declared**
  into SI, DI, DX respectively. The 2nd, 4th, 6th
  stay on stack despite meeting the threshold.
  
  So even though the register pool is {SI, DI,
  DX, BX, CX} (5 regs), BCC caps at **3 locals
  per function**, reserving BX and CX for **scratch
  use** (e.g., `[bx]` derefs, shift counts). When
  more variables qualify than slots available, BCC
  picks the **earliest-declared ones** to enregister.
- `1761` (**array parameter decays**): `int sum(int
  a[])` receives a **2-byte near pointer** at
  `[bp+4]`, not the array data. Callee uses `[si]`,
  `[si+2]`, `[si+4]` for element access (standard
  pointer arithmetic with constant offsets). Caller
  uses `lea ax, [x] / push ax` to pass the array's
  address. Confirms C's array-to-pointer decay at
  function boundaries.
- `1762` (**mutual recursion**): both functions
  in the same TU; the forward `int even(int n);`
  declaration lets `_odd` call `_even` before
  `_even`'s definition. The call sites use
  **`e8 imm16`** (relative call) with offsets
  computed at codegen time within the TU — **no
  EXTDEF** needed since both are local. PUBDEFs
  emitted for both _odd and _even.

Updated register allocation rule:
- Pool: {SI, DI, DX} (3 enregistration slots), BX
  and CX kept as scratch.
- Selection: variables with read-count ≥ 2 in
  source; if more than 3 qualify, take the
  **earliest-declared** 3.
- Spilled qualifying variables stay on stack with
  the same `[bp+disp]` access as un-qualifying
  locals.
- Address-taken / volatile / register keyword
  override the heuristic.

For the Rust reimplementation:
- Implement the use-count + declaration-order
  selection.
- Reserve BX, CX for transient ops (memory deref,
  shift counts).

## Global double no-init in BSS (8B); strlen loop pattern; `imul m16` for paren expr

Fixtures `1757` (uninitialized global double),
`1758` (strlen-like loop), and `1759` (paren
precedence `(a+b)*c`) cover three additional
shapes.

- `1757` (**global double in BSS**): an uninitialized
  `double g;` allocates **8 bytes in `_BSS`** —
  zero-initialized by the loader. The store/load
  uses **`fstp/fld qword [direct]`** (`9b dd /3
  disp16` and `/0 disp16`) with FIXUPP'd direct-
  memory addressing. ModR/M `1e` for store, `06`
  for load (mod=00 rm=110 = disp16-direct).
- `1758` (**strlen-like loop**): standard pattern
  for iterating over a null-terminated string:
  ```
  mov si, s_param      ; pointer
  xor di, di           ; n = 0
  jmp test
  body:
  inc di               ; n++
  inc si               ; s++
  test:
  cmp byte [si], 0     ; *s == 0?
  jne body             ; loop while non-zero
  ```
  Bottom-test pattern, byte load via `cmp byte [si],
  imm8` (`80 3c 00`, 3 bytes: opcode `80 /7` + ModR/
  M for [si] + imm8). Single-byte `inc si` for
  pointer advance.
- `1759` (**`(a + b) * c`**): paren grouping
  computes `a + b` first into AX, then **`imul
  word [bp-6]`** (`f7 /5` with mod=01 [bp+disp])
  multiplies AX directly by the memory operand —
  no separate load of c into a register. So memory
  operands work natively for `imul`:
  ```
  imul r/m16        ; f7 /5 + ModR/M + disp
  ```
  Same `f7` opcode group as `neg` (`/3`), `not`
  (`/2`), `div` (`/6`), `idiv` (`/7`), `mul` (`/4`),
  `imul` (`/5`).

For the Rust reimplementation:
- Globals: split on initializer presence — initd
  → `_DATA` LEDATA, uninitialized → `_BSS` size
  reservation.
- `imul`/`idiv` can take memory operands directly
  via mod=01/10 — no need to materialize the source
  into a register first.

## FP compares: `fcomp qword` + `jne`/`jb`/`ja`; double array stride 8

Fixtures `1754` (FP `==`), `1755` (double array
sum), and `1756` (FP `>=`) finalise the FP compare
encoding picture.

- `1754` (**FP `==`**): uses **`fcomp qword [b]`**
  (`dc /3`) — the double variant of fcomp (vs `d8
  /3` for float). The full sequence:
  ```
  fld qword [a]
  fcomp qword [b]      ; dc /3
  fstsw word [m]       ; save FPU status
  mov ax, [m]
  sahf                 ; copy AH → CPU flags
  jne L_false          ; 75 — branch if not equal
  ```
- `1755` (**double array stride 8**): `double a[3]`
  on stack lays elements at 8-byte stride. The
  `fld1` constant load is used for `1.0` (still
  cheaper than literal). Multi-fadd chain runs
  entirely on FPU stack — no intermediates spilled
  to memory.
- `1756` (**FP `>=`**): same fcomp+fstsw+sahf
  setup, but uses **`jb`** (`72`) for the false
  branch (`>=` true means CF=0; CF=1 means `<` is
  true so `jb` jumps to false). The FP-to-CPU flag
  mapping via `sahf`:
  | FPU state | C3 (→ZF) | C0 (→CF) | Triggered jcc |
  |-----------|----------|----------|---------------|
  | a > b | 0 | 0 | `ja` (above) |
  | a == b | 1 | 0 | `je`/`jae` |
  | a < b | 0 | 1 | `jb` (below) |
  | unordered | 1 | 1 | `jbe` |

  So source-level FP operators map to **unsigned-
  flavour jcc** in inverse form:
  | Operator | False-branch jcc |
  |----------|------------------|
  | `==` | `jne` (75) |
  | `!=` | `je` (74) |
  | `<`  | `jae` (73) |
  | `<=` | `ja` (77) |
  | `>`  | `jbe` (76) |
  | `>=` | `jb` (72) |

Updated FP-encoding summary:
| Op | Float | Double |
|----|-------|--------|
| fcomp m | `9b d8 /3` | `9b dc /3` |
| fcompp (stack) | `9b de d9` | (same) |
| fcom m | `9b d8 /2` | `9b dc /2` |

The fcom variants are for non-popping compares
(rare since BCC tends to use fcomp for compare-
and-pop in expressions).

## FP mul `dc /1`; `int + double` promotes via `fild`; FP negate via `fchs`

Fixtures `1751` (double*double), `1752` (int +
double mixed), and `1753` (FP negation) extend the
FP encoding catalogue.

- `1751` (**double mul**): `a * b` (both double)
  uses **`fmul qword [b]`** (`dc /1`). Same `dc`
  opcode group as add (`/0`), div (`/6`), reverse-
  div (`/7`).
- `1752` (**`int + double` promotion**): mixed-type
  expression promotes int to FP via **`fild word
  [i_temp]`** (`df /0`), then adds: `fadd qword
  [d]`. BCC copies `i` to a stack temp first (`mov
  ax, [i] / mov [tmp], ax`) because `fild` needs
  memory operand. So the C usual-arithmetic-
  conversion for int + FP happens at FPU-load
  time, not via a separate integer→FP conversion
  step.
- `1753` (**FP negate**): `-double` uses **`fchs`**
  (`d9 e0`, 2 bytes) — the FPU's change-sign
  instruction (flips sign bit of ST0). No memory
  access, no helper. Compares to integer negate
  `neg ax` (`f7 d8`, also 2 bytes).

Extended FP-opcode catalogue with these additions:
| Op | Encoding | Notes |
|----|----------|-------|
| `fld dword [m]` | `9b d9 /0` | float load |
| `fld qword [m]` | `9b dd /0` | double load |
| `fild word [m]` | `9b df /0` | int-as-FP load (sign-ext) |
| `fild dword [m]` | `9b db /0` | long-as-FP load (not yet probed) |
| `fld1`, `fldz` | `9b d9 e8/ee` | constant loads |
| `fstp dword [m]` | `9b d9 /3` | float store-pop |
| `fstp qword [m]` | `9b dd /3` | double store-pop |
| `fistp word [m]` | `9b df /3` | FP-to-int store (not yet probed) |
| `fadd dword [m]` | `9b d8 /0` | |
| `fadd qword [m]` | `9b dc /0` | |
| `fsub dword [m]` | `9b d8 /4` | |
| `fsub qword [m]` | `9b dc /4` | |
| `fmul dword [m]` | `9b d8 /1` | |
| `fmul qword [m]` | `9b dc /1` | |
| `fdiv dword [m]` | `9b d8 /6` | |
| `fdiv qword [m]` | `9b dc /6` | |
| `fchs` | `9b d9 e0` | change sign |
| `fcomp dword [m]` | `9b d8 /3` | compare-pop |
| `fstsw word [m]` | `9b dd /7` | save status word |
| `sahf` | `9e` (no wait) | flags ← AH |

So the FP encoding rule: opcode group selects
operand precision (`d8`=float-mem, `d9`=float
stack/const, `dc`=double-mem, `dd`=double
stack/store, `df`=int-mem), and `/N` selects the
specific operation within the group.

## `near` overrides model default; mixed near/far in same TU; dense switch always CS-relative

Fixtures `1748` (near ptr in -ml), `1749` (near fn
in -ml), and `1750` (dense switch in -ml) show how
explicit `near`/`far` qualifiers interact with the
memory model defaults.

- `1748` (**`near` ptr in -ml**): an explicit
  `int near *p` in large model produces a **2-byte
  pointer** with `mov ax, [si]` direct deref —
  same as the small-model default. The `near`
  qualifier **overrides** the model's far-data
  default. Useful for ptr-to-stack (which uses SS
  anyway and doesn't need a far ptr).
- `1749` (**`near` fn in -ml**): a `int near
  helper(int x)` in large model gets:
  - **near return** (`5d c3`) instead of `5d cb`
  - **args at `[bp+4]`** (not `[bp+6]`) because the
    return address is now near (2 bytes not 4)
  - **caller emits plain `call near`** (`e8`)
    without `push cs`
  
  So a single TU can have **mixed near and far
  functions**. The compiler tracks each function's
  ABI based on its declaration and emits the
  correct call/ret pair. main is far (model default)
  while helper is near.
- `1750` (**dense switch in -ml**): the indexed-
  table dispatch uses **`cs:[bx + offset]`** (`2e
  ff /4`) — **identical to small model**. CS-
  relative addressing doesn't depend on the data
  model since the jump table lives in the code
  segment. Both small and large models produce
  byte-identical switch dispatch sequences (modulo
  the surrounding function ABI bytes).

So the model interaction is per-function-symbol-level:

| Qualifier | Effect in -ms | Effect in -ml |
|-----------|---------------|---------------|
| (default) | near (matches model) | far (matches model) |
| `near` fn | near (same) | **near** (overrides) |
| `far` fn | **far** (overrides) | far (same) |
| (default) ptr to data | near (matches model) | far (matches model) |
| `near *` | near (same) | **near** (overrides) |
| `far *` | **far** (overrides) | far (same) |

This per-function tracking is important for the
Rust reimplementation:
- Track `ABI = near|far` per fn-decl symbol based
  on qualifier + model default.
- Emit `push cs` + `e8` for far-call sites; just
  `e8` for near-call sites.
- Emit `ret` (`c3`) or `retf` (`cb`) in epilogue
  per function's own ABI flag.
- Param offsets: `bp+4` (near) or `bp+6` (far).

## -ml: params at `[bp+6]+`; long add unchanged; mul unchanged; struct call uses `push cs`

Fixtures `1745` (long add in -ml), `1746` (struct
by value in -ml), and `1747` (mul by 17 in -ml)
extend the large-model coverage and confirm cross-
model orthogonality.

- `1745` (**long add in large model**): the inline
  `add low / adc high` sequence is **byte-identical**
  to small model — the only OBJ-level difference is
  `5d cb` (retf) instead of `5d c3` (ret) in the
  epilogue. Long arithmetic helpers (which would
  appear as EXTDEFs) would also be unchanged
  names. So IR-level long-op encoding is fully
  model-independent.
- `1746` (**small struct by value in -ml**): the
  decomposition into 2 word pushes is the same,
  but the param offsets shift:
  ```
  small (-ms):  arg1 at [bp+4], arg2 at [bp+6]
  large (-ml):  arg1 at [bp+6], arg2 at [bp+8]
  ```
  The +2 shift accounts for the **4-byte far
  return address** (seg + off) on the stack instead
  of 2 bytes. Call site uses the **`push cs ;
  call near`** 4-byte sequence (vs 3-byte `call
  near` in small).
- `1747` (**mul by 17 in -ml**): `mov dx, 17 /
  imul dx` is byte-identical to small model except
  the `5d cb` retf. Confirms integer arithmetic
  operations are fully model-independent.

So the cross-model parameter rules:
| Slot | Small (`-ms`) | Large (`-ml`) |
|------|---------------|---------------|
| saved BP | [bp+0..1] | [bp+0..1] |
| return addr | [bp+2..3] (near) | [bp+2..5] (far) |
| arg1 | [bp+4..5] | [bp+6..7] |
| arg2 | [bp+6..7] | [bp+8..9] |
| ... | each +2 from arg1 | each +2 from arg1 |

For the Rust reimplementation:
- Parameterize `arg_offset_base = (small ? 4 : 6)`
  in the codegen.
- The `near` vs `far` ABI is purely an emission-
  layer concern — the parser/AST stays the same.
- Adding new -ml fixtures cheaply verifies that
  the encoder's model parameter works correctly
  by re-running the same C source under different
  flags.

## Sparse switch search-table CS-relative; block scope reuses slots; typedef fn-ptr identical

Fixtures `1742` (sparse switch large base), `1743`
(block-scoped declarations), and `1744` (typedef
fn pointer) cover three remaining shapes.

- `1742` (**sparse switch with large base**):
  Confirmed the **search-table dispatch strategy**:
  ```
  mov ax, x
  mov [scrutinee_slot], ax
  mov cx, N_cases       ; loop count
  mov bx, &case_value_table
  loop_start:
  mov ax, cs:[bx]       ; 2e 8b 07 — read case value from CS
  cmp ax, [scrutinee]
  je dispatch           ; 74 06 — short forward
  inc bx ; inc bx       ; advance 2 bytes
  loop loop_start       ; e2 f4 — dec cx, jump if non-zero
  jmp default
  dispatch:
  jmp word ptr cs:[bx + 2*N_cases]
  ```
  Key insights:
  - **`2e` CS-override prefix** is used for table reads — case values and offsets are stored in code segment (right after the dispatch code).
  - **`loop`** instruction (`e2 rel8`) drives the iteration — single instruction handles dec+jcc.
  - **Two parallel tables**: N case values followed by N target offsets, indexed via `[bx + 2*N]` adjustment.
  - Used when N ≥ 4 and cases are sparse (non-dense).

  This is distinct from the indexed-table strategy (dense cases) which uses `(scrutinee - base) * 2` as direct table index without a search loop.
- `1743` (**block scope reuses stack slots**): a
  nested `{ }` block's locals can reuse slots that
  earlier (now-out-of-scope) locals occupied:
  ```
  Block 1: a at [bp-2], b at [bp-4]
  Block 2: c at [bp-2]    ← reuses a's slot!
  ```
  Total frame is only 4 bytes (2 + 2 = 4 for the
  max in-scope at any point) instead of 6 (a + b
  + c). BCC tracks variable lifetimes via lexical
  scope and recycles slots.
- `1744` (**typedef fn-ptr identical to direct**):
  `typedef int (*op_t)(int); op_t f = dbl; f(7)`
  produces **byte-identical** code to `int (*f)(int)
  = dbl; f(7)`. `typedef` for function-pointer
  types is purely syntactic. Indirect call uses
  `ff 56 disp` (`ff /2` call near r/m16 with
  bp-relative address).

For the Rust reimplementation:
- Track variable lifetimes during AST scope
  analysis; assign stack offsets after a "max
  concurrent live set" pass to enable slot reuse.
- Switch dispatch strategy selection:
  - 1-3 cases → linear `cmp/je` chain
  - 4+ cases, dense → indexed table
  - 4+ cases, sparse → search-table with `loop` +
    `cs:[bx]` reads
- typedef fully resolved at parse time, never
  reaches codegen.

## Large frame uses `sub sp, imm16` + disp16; switch-default-only no dispatch; EXTDEF for extern

Fixtures `1739` (200-byte stack frame), `1740`
(switch with only default), and `1741` (extern fn
decl) close out the call/frame/dispatch picture.

- `1739` (**large stack frame**): a 200-byte local
  array forces use of **`sub sp, 200`** via the
  `81 /5 imm16` form (4 bytes total: `81 ec c8
  00`). Then `mov [bp-200], imm` uses **disp16
  addressing** (mod=10, `c7 86 38 ff 07 00`) — the
  offset `0xff38` = -200 in two's complement
  doesn't fit imm8.

  **Stack frame allocation tiers**:
  | Size N | Encoding | Bytes |
  |--------|----------|-------|
  | N=1 | `dec sp` (`4c`) | 1 |
  | N=2 | `dec sp ; dec sp` | 2 |
  | 3 ≤ N ≤ 127 | `sub sp, imm8` (`83 ec imm8`) | 3 |
  | N > 127 | `sub sp, imm16` (`81 ec imm16`) | 4 |

  **Stack frame addressing tiers** for `[bp+disp]`:
  | |disp| | Encoding (mod) | Bytes |
  |--------|--------------|-------|
  | 0 | mod=00 (rare) | (no disp) |
  | ≤ 127 | mod=01 disp8 | 1 extra |
  | > 127 | mod=10 disp16 | 2 extra |

  So `mov [bp+disp8], imm16` = 4 bytes, `mov
  [bp+disp16], imm16` = 5 bytes. Same instruction,
  larger displacement field.
- `1740` (**switch with default-only**): emits **no
  dispatch table** — just `jmp short 0` (`eb 00`)
  followed by the default body. Since there are
  no case labels, no scrutinee comparison is needed
  — execution always falls into default. The
  `eb 00` is a 2-byte break-target placeholder for
  the switch's end label.
- `1741` (**`extern` declared, not defined**): only
  emits an **EXTDEF record** for the function. The
  call site uses **`e8 imm16`** (near relative call)
  with `0x0000` placeholder; the linker resolves
  via FIXUPP at link time. No code body emitted for
  the external. Confirms symbol-linkage categories
  ([[batch-463-static-fn]]).

Final symbol-linkage / emit categories:
| Category | OBJ output |
|----------|------------|
| `extern` (default) fn, defined | PUBDEF + emit + EXTDEF (for callers) |
| `static` fn, defined | emit only (no PUBDEF) |
| `extern` fn, declared not defined | EXTDEF only |
| Local automatic | (no symbol output) |
| `extern` global, defined | PUBDEF + LEDATA/BSS |
| `static` global, defined | LEDATA/BSS only |
| `extern` global, declared only | EXTDEF only |

## Long `>>1` = `sar/rcr`; `long == 0` = OR halves; `-long` = neg/neg/sbb

Fixtures `1736` (long `>>1` inline), `1737` (long
== 0 shortcut), and `1738` (long negation) complete
the inline long-op characterisation.

- `1736` (**long `>>1` inline**): signed `>> 1` on
  a long is **inlined** as:
  ```
  sar high, 1    ; d1 f8 — arith shift right
  rcr low, 1     ; d1 da — rotate carry right
  ```
  The `sar` shifts high right with sign preserved
  and puts the low bit of high into CF. `rcr` then
  rotates that CF into the top bit of low. So the
  full 32-bit signed `>>1` is **2 instructions**.
  Mirrors the `<<1` inline pattern (`shl/rcl`):
  - `<<1`: `shl low / rcl high` (carry low→high)
  - `>>1` (signed): `sar high / rcr low` (carry
    high→low, sign preserved)
  - `>>1` (unsigned): `shr high / rcr low` (not
    yet probed)
- `1737` (**long == 0 shortcut**): `if (a == 0)`
  for a `long` uses the **OR-halves trick**:
  ```
  mov ax, a_low
  or ax, a_high      ; ZF = (low | high) == 0
  jne L_false
  ```
  Both halves OR'd into AX in one instruction; ZF
  tests if all 32 bits are zero. **Much cheaper**
  than the general 2-step long compare. Specific to
  comparing against zero (both `==` and `!=`).
- `1738` (**long negation `-a`**): inlined as
  3 instructions:
  ```
  neg high       ; f7 d8 — negate high
  neg low        ; f7 da — negate low, CF=1 if low!=0
  sbb high, 0    ; 1d 00 00 — high -= CF (borrow propagation)
  ```
  Result: properly negated 32-bit value with carry
  propagation between halves. Note the AX/DX
  register roles in BCC's inline long ops:
  **AX = HIGH, DX = LOW** for these in-flight
  operations (opposite of the long return ABI's
  DX = HIGH, AX = LOW).

Inline long-op catalogue (all 4 bytes or less for
the core operation):
| Op | Sequence | Bytes |
|----|----------|-------|
| `a + b` | `add low / adc high` | 4 (with mem ops) |
| `a - b` | `sub low / sbb high` | 4 |
| `-a` | `neg high / neg low / sbb high, 0` | 7 |
| `a == 0` | `mov ax, low / or ax, high` | 5 |
| `a << 1` | `shl low / rcl high` | 4 |
| `a >> 1` (s) | `sar high / rcr low` | 4 |
| `a & b`, `|`, `^` | `op low, low / op high, high` | 4 |
| `a == b` | `cmp high / jne / cmp low / jne` | varies |

Long shifts by N>1 always use `N_LXLSH@`/`N_LXRSH@`
helpers; shift-by-1 is the special inline case.

## `*long_p = K` = 2 word stores; printf cdecl R-to-L; long `<<1` inline `shl/rcl`

Fixtures `1733` (long pointer deref-store), `1734`
(printf variadic call), and `1735` (long shift-by-1
inlined) close several remaining shapes.

- `1733` (**writing a long through a pointer**):
  emits **two word stores** through the pointer
  with `[si]` and `[si+2]` addressing:
  ```
  mov word [si+2], 0x000f    ; high half — c7 44 02 0f 00
  mov word [si],   0x4240    ; low half — c7 04 40 42
  ```
  The 32-bit constant is split into two 16-bit
  imm16s at parse time; each half stored to its
  word slot. No N_SCOPY@ needed — long is just two
  word writes.
- `1734` (**variadic printf**): a vararg call uses
  **standard cdecl R-to-L push** with caller
  cleanup:
  ```
  mov ax, 42
  push ax            ; arg 2 first (rightmost)
  mov ax, &"%d\n"    ; FIXUPP'd to data
  push ax            ; arg 1 (fmt)
  call _printf       ; FIXUPP'd external call
  pop cx / pop cx    ; cleanup 4 bytes
  ```
  Caller-cleanup is essential for variadic — the
  callee doesn't know the arg count, so it can't
  do callee-cleanup. **All cdecl functions can be
  variadic** because the protocol is the same.
- `1735` (**long `<<1` inlined**): a long shift-by-
  1 is **inlined** as `shl low / rcl high` — uses
  the carry flag to propagate the shifted-out bit
  from low half to low bit of high half:
  ```
  shl dx, 1          ; d1 e2 — low << 1, CF = top bit
  rcl ax, 1          ; d1 d0 — high << 1 with CF in low bit
  ```
  Total 4 bytes for the shift core (vs ~8 bytes
  for calling `N_LXLSH@`). Long shift-by-1 is the
  **only inlined long shift**; shift-by-N (N>1)
  still uses `N_LXLSH@` ([[batch-440-long-shifts]])
  even for constant N.

For the Rust reimplementation:
- Long pointer-store splits constants at parse time
  into low/high words.
- Variadic call signatures are codegen-identical
  to fixed-arity cdecl — no special protocol.
- Long shift-by-1 should be inlined as `shl/rcl`;
  shift-by-N for N≥2 emits the helper call.

## `int→char` = byte load; `uchar→int` = `mov ah,0`; `schar→int` = `cbw`

Fixtures `1730` (int→char cast), `1731` (uchar→int
zero-extend), and `1732` (signed char→int sign-
extend) characterise the char/int conversion
codegen.

- `1730` (**`(char)int` narrowing**): no explicit
  shift or mask — just **`mov al, byte [m]`** to
  read the low byte. The 1-byte store `mov [c], al`
  (`88 /N`) then commits. Truncation is implicit
  in the byte-width access. So narrowing is **free**:
  ```
  mov al, byte [int_var]    ; 8a /N (read low byte)
  mov [char_var], al         ; 88 /N (store byte)
  ```
- `1731` (**`(int)unsigned char` zero-extend**):
  uses **`mov ah, 0`** (`b4 00`, 2 bytes) to zero
  the high byte after `mov al, [uc]`. The
  combination yields AX with low byte = uc and high
  byte = 0.
- `1732` (**`(int)signed char` sign-extend**): uses
  **`cbw`** (`98`, 1 byte) — convert byte to word
  by sign-extending AL into AH. Mirrors the int→
  long pattern (`cwd`):
  - `cbw` (`98`) — AL → AX (sign extend)
  - `cwd` (`99`) — AX → DX:AX (sign extend)

So the widening conversions follow the same per-
signedness rule across all widths:
| Conversion | Signed | Unsigned |
|------------|--------|----------|
| `char → int` | `cbw` (1B) | `mov ah, 0` (2B) |
| `int → long` | `cwd` (1B) | `mov word [high], 0` (5B) |

**Char-init byte instructions**:
- `mov byte [m], imm8` = `c6 /N + disp + imm8`
- `mov al, byte [m]` = `8a /N`
- `mov byte [m], al` = `88 /N`

So initing a char with a constant uses `c6 imm8`
(3-4 bytes), distinct from `c7 imm16` for words.

This completes the char/int conversion picture.
Char arithmetic always happens at int width (via
implicit promotion); narrowing back to char drops
the high byte without explicit work.

## Var-shift = `mov cl,[byte] / shr/sar`; ++x vs x++ order; string concat parse-time

Fixtures `1727` (shift by var), `1728` (pre vs post
inc as rvalue), and `1729` (adjacent string
literal concat) cover three remaining shapes.

- `1727` (**shift by variable**): `x >> n` lowers
  to:
  ```
  mov ax, x          ; 8b 46 disp
  mov cl, byte [n]   ; 8a 4e disp - LOW BYTE only
  sar ax, cl         ; d3 f8 - signed shift
  ```
  Notable: `mov cl, byte [n]` (`8a /N`) loads only
  the low byte — saves 1 byte since shift amount
  uses only CL's low 5 bits anyway. And **signed
  `>>` uses `sar`** (`d3 /7`), unsigned would use
  `shr` (`d3 /5`). Same opcode group, signedness
  in /N.
- `1728` (**pre vs post inc as rvalue**):
  - `a = ++x` emits **`inc si / mov ax, si`** —
    inc FIRST, then capture NEW value.
  - `b = y++` emits **`mov ax, di / inc di`** —
    capture OLD value FIRST, then inc.
  Both leave the variable incremented; the
  difference is which value is captured into the
  destination. The opcode order at the bytestream
  level directly encodes the C semantics.
- `1729` (**adjacent string literal concat**):
  `"AB" "CD"` becomes **a single literal "ABCD"**
  (5 bytes `41 42 43 44 00`) in `_DATA`. The two
  literals are **joined at parse time** with one
  null terminator. Standard C89 spec behavior.
  Code accesses via `mov al, [si+2]` for `s[2]`,
  resolving to 'C'.

Var-shift encoding catalog:
| Op | Encoding |
|----|----------|
| `shl ax, cl` (any) | `d3 e0` |
| `shr ax, cl` (unsigned) | `d3 e8` |
| `sar ax, cl` (signed) | `d3 f8` |
| `mov cl, byte [m]` | `8a 4e disp` |
| `mov cl, byte [reg]` | `8a 0?` (depends on reg) |

## Unsigned mod-pow2 = AND mask; shift count ≥3 uses CL-form; signed mod = full idiv

Fixtures `1724` (unsigned mod by 8), `1725`
(unsigned div by 256), and `1726` (signed mod by
8) finalise the div/mod codegen rules.

- `1724` (**unsigned mod by pow2 = AND mask**): `x
  % 8` (unsigned) lowers to **`and ax, 7`** (`25
  07 00`, 3 bytes via AX-imm16 form). This is the
  optimal pow2-mod shortcut: `x % 2^N = x & (2^N -
  1)`. Confirms the asymmetric optimisation —
  unsigned mod by pow2 is **one instruction**.
- `1725` (**unsigned div by large pow2 uses CL-
  form**): `x / 256` (unsigned) = `x >> 8` lowers
  to **`mov cl, 8 / shr ax, cl`** (4 bytes). For
  shift count N, the unrolled form costs `2*N`
  bytes; CL-form is fixed 4 bytes. So the boundary
  is:
  - N = 1: unrolled (2 bytes) — single `shr ax, 1`
  - N = 2: unrolled (4 bytes) — two `shr ax, 1`
    (matches CL-form bytes)
  - N ≥ 3: CL-form (4 bytes wins)
  Empirically BCC uses unrolled for N=2 ([[batch-
  468-div-mod]]) and CL-form for N=8. The exact
  threshold appears to be **N ≥ 3 → CL-form**.
- `1726` (**signed mod by pow2 = full idiv**): `x
  % 8` (signed) uses **the full `idiv` sequence**,
  NOT the AND mask. Reason: AND gives unsigned 0..7,
  but signed mod can be negative for negative
  dividends (`-7 % 8 = -7`). BCC plays it safe with
  idiv for all signed mod regardless of divisor.
  Code: `mov bx, 8 / cwd / idiv bx / mov [r], dx`
  — 9 bytes.

So the **asymmetric pow2 optimisation** is now
fully characterised:
| Op | Pow2-N | Bytes | Encoding |
|----|--------|-------|----------|
| unsigned `x / 2^N` | 1 | 2 | `shr ax, 1` |
| unsigned `x / 2^N` | 2 | 4 | `shr ax, 1` × 2 |
| unsigned `x / 2^N` | ≥3 | 4 | `mov cl, N / shr ax, cl` |
| unsigned `x % 2^N` | any | 3 | `and ax, K-1` |
| signed `x / 2^N` | any | 8 | `mov bx, K / cwd / idiv bx` |
| signed `x % 2^N` | any | 9 | (same idiv, read DX) |

Signed division never benefits from pow2 shortcuts
due to C's truncation-toward-zero semantics. The
9+ byte idiv sequence is the floor for signed
div/mod.

## Signed div-by-pow2 uses `idiv` (NOT `sar`); unsigned uses unrolled `shr`; mod reads DX

Fixtures `1721` (signed div by 4 = pow2), `1722`
(unsigned div by 4), and `1723` (signed mod by 7)
clarify the div/mod codegen rules.

- `1721` (**SIGNED div by pow2 — full `idiv`!**):
  even for divisor 4, signed `x / 4` uses
  **`mov bx, 4 / cwd / idiv bx`** — NOT `sar`. The
  rationale: `sar` rounds toward negative infinity,
  but C signed `/` rounds toward zero (truncation).
  For negative dividends, `sar` would give wrong
  results (e.g., `-7 sar 1` = -4 but `-7 / 2` = -3).
  So BCC plays it safe with idiv for signed
  division by any constant, pow2 or not.
- `1722` (**unsigned div by pow2 — unrolled
  `shr`**): unsigned `x / 4` uses **`shr ax, 1`
  twice** (`d1 e8 d1 e8`, 4 bytes total). BCC
  unrolls the shift for small N rather than using
  `mov cl, N / shr ax, cl`. For N ≥ some threshold
  (probably ≥ 4 or 5), it switches to the cl-based
  form. Unsigned semantics correctly match `shr`
  (round toward 0 = floor for positive numbers).
- `1723` (**signed mod uses idiv, reads DX**):
  signed `x % 7` is the **same idiv sequence** as
  signed div, but **stores DX** (the remainder)
  instead of AX (the quotient). The two operations
  share the entire computational path:
  ```
  mov ax, x
  mov bx, K
  cwd
  idiv bx
  ; → AX = quotient, DX = remainder
  mov [r], dx        ; for mod
  ; OR
  mov [r], ax        ; for div
  ```
  So BCC emits one `idiv` and picks the output
  register at the consumer. If both `x/K` and `x%K`
  appeared in the same expression, BCC could
  theoretically fuse them — not yet probed.

So the **div/mod encoding rule** by signedness and
divisor shape:
| Operation | Divisor | Encoding |
|-----------|---------|----------|
| signed `/` | const pow2 | `idiv bx` (cannot use sar) |
| signed `/` | const non-pow2 | `idiv bx` |
| signed `/` | variable | `idiv bx` |
| unsigned `/` | const pow2 | `shr ax, 1` × N (unrolled) |
| unsigned `/` | const non-pow2 | `xor dx, dx / div bx` |
| unsigned `/` | variable | `xor dx, dx / div bx` |
| signed `%` | const pow2 | `idiv bx` (read DX) |
| signed `%` | any | `idiv bx` (read DX) |
| unsigned `%` | const pow2 | `and ax, K-1` (mask shortcut!) |
| unsigned `%` | other | `xor dx,dx / div bx` (read DX) |

Note the asymmetric optimisation: **unsigned mod
by pow2 = AND mask** (cheapest), but **signed mod
by pow2** can't use the mask because of negative
numbers, so falls back to full idiv.

## `cmp m, 0x1234` uses `81 /7` (imm16); `x*K` via DX+imul; `x/K` via cwd+idiv

Fixtures `1718` (cmp imm16), `1719` (mul by non-
pow2 17), and `1720` (signed div by 3) confirm
arithmetic encoding shapes for the encoding-policy
boundary cases.

- `1718` (**cmp with imm16**): `if (x == 0x1234)`
  emits **`81 7e fe 34 12`** (`cmp word [bp-2],
  0x1234`) — the full `81 /7` imm16 form (5 bytes).
  Since 0x1234 doesn't fit in imm8-sext (which
  would sign-extend `0x12` byte to `0x0012`), the
  imm16 encoding is **required**. Reconfirms the
  imm8-sext policy from earlier batches.
- `1719` (**mul by non-pow2 17**): lowers to:
  ```
  mov ax, x         ; 8b 46 fe
  mov dx, 17        ; ba 11 00
  imul dx           ; f7 ea — signed mul ax * dx → dx:ax
  store ax → r
  ```
  Constant goes to **DX** then `imul dx` does the
  signed multiply via the implicit-AX form (`f7
  /5`). Result low half in AX. No `mul` (unsigned)
  used here — BCC always emits `imul` per the
  signedness rule.
- `1720` (**signed div by 3**): lowers to:
  ```
  mov ax, x         ; 8b 46 fe
  mov bx, 3         ; bb 03 00
  cwd               ; 99 — sign-extend AX→DX:AX
  idiv bx           ; f7 fb — signed div DX:AX / BX → AX=quot
  store ax → r
  ```
  The divisor goes to **BX**. **`cwd`** is required
  before `idiv` to properly sign-extend the
  16-bit AX into the 32-bit DX:AX dividend. The
  `idiv` operates on a 32-bit dividend / 16-bit
  divisor → 16-bit quotient. Compare to **unsigned**
  div which would use `xor dx, dx / div bx` (the
  ZF-extension version) instead of `cwd / idiv`.

Updated arithmetic-with-constant table:
| Operation | Encoding | Notes |
|-----------|----------|-------|
| `x + K` (small) | `inc ax` / `83 /0 imm8` | imm8-sext if fits |
| `x + K` (large) | `05 imm16` (AX) or `81 /0 imm16` | full imm16 |
| `x * 2^N` | `shl ax, N` | pow2 shortcut |
| `x * K` (non-pow2) | `mov dx, K / imul dx` | always imul |
| `x / 2^N` (unsigned) | `shr ax, N` | pow2 shortcut |
| `x / 2^N` (signed) | (likely `sar`) | not yet probed for general N |
| `x / K` (signed) | `mov bx, K / cwd / idiv bx` | helper-free |
| `x / K` (unsigned) | `mov bx, K / xor dx,dx / div bx` | helper-free |
| `cmp x, K` (small) | `83 /7 imm8` | imm8-sext |
| `cmp x, K` (large) | `81 /7 imm16` | full imm16 |

So integer divide/multiply are entirely inline (no
helpers like the long-arithmetic ones); only long
ops use helpers.

## `x &= K` via `81 /N reg, imm16`; ternary = jcc + 2 movs; nested calls inner-first

Fixtures `1715` (bitwise compound assign), `1716`
(min via ternary), and `1717` (nested function
calls) cover three remaining shapes.

- `1715` (**bitwise compound on register**): `x
  &= 0x0f0f` on SI-resident x lowers directly to
  **`and si, 0x0f0f`** (`81 e6 0f 0f`, 4 bytes).
  Same shape for `|=` (`81 ce ...`) and `^=` (`81
  f6 ...`). No AX round-trip — the register-with-
  imm16 form modifies the register in place.
  - `81 /4 reg, imm16` = AND (4 bytes)
  - `81 /1 reg, imm16` = OR
  - `81 /6 reg, imm16` = XOR
  Each takes 4 bytes vs the alternative `mov ax, si
  / and ax, K / mov si, ax` (8 bytes). So compound
  assign on register locals is the cheap path
  whenever the constant doesn't fit `imm8-sext`
  (which AND/OR/XOR don't use anyway per the
  encoding policy — [[batch-407-imm8-sext-policy]]
  notes that AND/OR/XOR always use `81 /N` imm16
  form).
- `1716` (**ternary `a<b ? a : b`**): lowers to:
  ```
  cmp si, di         ; a vs b
  jge L_else         ; inverse condition (>= used as NOT <)
  mov ax, si         ; then branch: a
  jmp L_done
  L_else:
  mov ax, di         ; else branch: b
  L_done:
  mov [m], ax        ; store result
  ```
  Note: the **inverse condition** `jge` is used to
  skip the "then" branch when `a < b` is false. So
  the test selects the **opposite** of the source-
  level operator. Both branches materialize into AX,
  then a single store lands the result.
- `1717` (**nested call `sqr(inc(4))`**): evaluates
  **inner-first**: push 4, call inc, pop, push AX,
  call sqr, pop. The intermediate result in AX is
  reused directly as the outer call's argument via
  `push ax` after the inner `pop cx`. No temporary
  spill to stack/memory for the intermediate.
  Sequence:
  ```
  mov ax, 4
  push ax            ; inc's arg
  call _inc
  pop cx             ; cleanup inc's arg
  push ax            ; sqr's arg (= inc's return)
  call _sqr
  pop cx
  ```

These three round out the basic codegen catalogue
— compound assign forms, ternary boolean-select,
and nested function call sequencing all confirmed.

## `char s[] = "..."` runtime-copies via `N_SCOPY@`; `!x` = `neg/sbb/inc` idiom

Fixtures `1712` (`char s[] = "ABC"`), `1713`
(global int array with initializer), and `1714`
(`!!x` double negation) cover three init/op shapes.

- `1712` (**char array init from string literal**):
  uses `N_SCOPY@` to copy the literal from `_DATA`
  to the local stack array at runtime. NOT a
  static initializer — the array is dynamically
  populated each time the function runs. Sequence:
  ```
  push ss / lea ax, [bp+dst] / push ax    ; dest fp
  push ds / mov ax, &literal / push ax    ; src fp
  mov cx, 4                                ; length + 1
  call N_SCOPY@                            ; copies
  ```
  String literal "ABC" is stored in `_DATA` as
  `41 42 43 00` (4 bytes including null).
- `1713` (**global array initialized**): a file-
  scope `int a[] = {10, 20, 30}` places the data
  **directly in `_DATA`** (not BSS) as `0a 00 14
  00 1e 00`. No N_SCOPY@, no init code — the
  values are baked into the OBJ's LEDATA records
  and loaded by the OS image loader. Element access
  in code uses `add ax, [_a + 2*i]` (`03 06 disp16`)
  via FIXUPP-resolved direct memory operands.
- `1714` (**`!x` boolean idiom**): lowers to a
  **3-instruction sequence** with the canonical
  8086 boolean-ize pattern:
  ```
  neg ax        ; f7 d8 - sets CF=1 if AX != 0
  sbb ax, ax    ; 1b c0 - AX = -CF, so 0xffff or 0
  inc ax        ; 40    - flip: 0 if was non-zero
                ;              1 if was zero
  ```
  Total: 5 bytes. The `neg / sbb / inc` idiom
  converts any non-zero value to 0, zero to 1.
  `!!x` applies this twice, which has the effect of
  converting any value to a clean 0-or-1 boolean.
  No special handling — BCC doesn't recognise
  `!!` as a fold opportunity, it just emits the
  sequence twice.

Two distinct array-init paths now characterised:
| Source | Mechanism |
|--------|-----------|
| local `char s[] = "lit"` | runtime `N_SCOPY@` |
| local `int a[3] = {1,2,3}` (small) | inline `mov [m], imm` stores |
| local `int a[10] = {…}` (large) | `N_SCOPY@` (already seen) |
| global `int a[] = {1,2,3}` | direct `_DATA` placement (no init code) |
| global `char s[] = "ABC"` | direct `_DATA` placement (no init code) |

The boundary for local-aggregate init: small (~4
words?) gets inline stores; larger uses N_SCOPY@
from a template in _DATA.

For the Rust reimplementation:
- Choose between inline init stores and N_SCOPY@
  based on aggregate size at the per-declaration
  level.
- Place global initializers directly in `_DATA`
  LEDATA records.
- Implement the `!x` lowering as the
  `neg / sbb ax,ax / inc ax` idiom (5 bytes)
  rather than `cmp ax, 0 / sete al / movzx`.

## `sizeof` fully folded; string literals packed in `_DATA`; `<<0` `>>0` `|0` no-ops

Fixtures `1709` (sizeof folded), `1710` (array of
string pointers), and `1711` (shift/or by zero)
characterise three constant-folding and storage
shapes.

- `1709` (**`sizeof` is a compile-time constant**):
  the entire expression `sizeof(int) + sizeof(char)
  + sizeof(long)` (= 2+1+4=7) folds to a single
  `mov ax, 7`. The function body is **3 bytes**.
  Confirms the type sizes:
  - `char` = 1 byte
  - `int` = 2 bytes
  - `long` = 4 bytes
  - (FP not yet probed for sizeof — `float` = 4,
    `double` = 8)
- `1710` (**string literals packed**): multiple
  string literals `"AB"`, `"CD"`, `"EF"` are
  **concatenated sequentially in `_DATA`** with
  null terminators (= 9 bytes: `41 42 00 43 44 00
  45 46 00`). Each `strs[i] = "..."` initialization
  stores the offset to that literal's start (with
  FIXUPP) — `strs[0]` gets offset 0, `strs[1]`
  gets 3 (after "AB\0"), etc. So **deduplication
  isn't performed** — distinct literal text means
  distinct storage. The store instruction emits
  the displacement as the FIXUPP target; the
  linker resolves to actual data segment offset.
- `1711` (**identity-shift/OR folded**):
  - `x << 0` → just `x` (no shift instructions)
  - `x >> 0` → just `x`
  - `x | 0` → just `x`
  All three assignments lower to **simple stores**
  of x's value, without the corresponding bitwise/
  shift operation. For `c = x | 0` where x is in
  SI, BCC emits **`mov [c], si`** directly (3
  bytes) — bypassing the usual `mov ax, src / mov
  dst, ax` two-instruction pattern. This is a
  "register-to-memory direct store" shortcut for
  same-register-as-source cases.

Constant folding catalogue confirmed:
| Identity | Folded to |
|----------|-----------|
| `x + 0` / `0 + x` | `x` |
| `x - 0` | `x` |
| `x * 0` | `0` |
| `x * 1` | `x` |
| `x * 2^N` | `shl x, N` |
| `x << 0` / `x >> 0` | `x` |
| `x | 0` / `x ^ 0` | `x` |
| `x & 0` | `0` |
| `x & 0xFFFF` (full mask) | `x` |
| `sizeof(T)` | `sizeof(T)` constant |
| `(int_const) op (int_const)` | full evaluation |

So BCC has a comprehensive parse-time constant
folding pass that handles all the standard
identities — important for the Rust
reimplementation to replicate exactly.

## Array-of-struct linearised; 5-arg uses `add sp, 10`; `static` fn omits PUBDEF

Fixtures `1706` (array of struct), `1707` (5-arg
function), and `1708` (static function) cover three
codegen-affecting cases.

- `1706` (**array of struct**): `struct P a[2]`
  lays out as `a[0].x, a[0].y, a[1].x, a[1].y` —
  flat linear layout, each field accessible as
  `[bp+disp]`. For constant indices, the disp is
  baked in at parse time. The subtract `a[1].x +
  a[1].y - a[0].x` uses **`sub ax, [m]`** (opcode
  `2b /N`) — direct memory subtract, no separate
  load needed.
- `1707` (**5-arg cdecl**): args at `[bp+4]` to
  `[bp+12]` in declaration order. Caller cleans
  with **`add sp, 10`** (5 args × 2 bytes). The
  `add sp, N` encoding (`83 c4 disp8` for N ≤ 127)
  is 3 bytes regardless of N, so it's always more
  efficient than ≥ 3 individual `pop cx` (also
  1 byte each but with overhead) for any cleanup
  ≥ 6 bytes. Confirms the cleanup-strategy boundary
  from [[batch-435-arg-cleanup-boundary]].
- `1708` (**`static` function**): the OBJ has
  **no PUBDEF for `_helper`** — only `_main` is
  exported. Static linkage means the symbol stays
  internal; same-TU calls resolve via relative
  offsets in the call's `e8` displacement. The
  function bytes are still emitted to `_TEXT`,
  just not exported.

For the Rust reimplementation:
- Track per-symbol linkage flag (extern vs static).
- Emit `PUBDEF` records only for `extern`
  (default) functions and globals.
- `static` declarations still need their bytes
  emitted to the appropriate segment, but no
  external visibility — the linker won't see
  them.

This means each TU has 4 categories:
1. Default `extern` (PUBDEF + emit)
2. `static` (no PUBDEF, just emit)
3. `extern` declaration without definition (EXTDEF
   only, no emit)
4. Local automatic (no record at all — lives
   on stack)

## `break` jumps to epilogue; `continue` jumps to post-update; `test` for bit check

Fixtures `1703` (do-while + break), `1704` (for +
continue), and `1705` (multi-decl init) cover three
control-flow shapes.

- `1703` (**`break` inside loop**): emits a
  **direct `jmp` to the loop epilogue** (or past
  the loop's test/end). Bypasses the loop
  condition entirely. The shape:
  ```
  ; loop body
  cmp di, 5         ; sum > 5?
  jle continue
  jmp break_target  ; -> after loop
  continue:
  cmp si, 10        ; loop test
  jl body
  break_target:
  ```
  So `break` is a one-byte `jmp short` for nearby
  loops, or `jmp near` (3 bytes) for distant ones.
- `1704` (**`continue`**): emits a **`jmp` to the
  loop's post-update / test step**, NOT to the
  loop body. So `continue` skips the rest of the
  body but still triggers `i++` (in a for loop)
  and re-tests the loop condition. The shape:
  ```
  body:
  test si, 1        ; check i & 1
  jz no_skip
  jmp continue_pt
  no_skip:
  add di, si        ; rest of body
  continue_pt:
  inc si            ; for's post-update
  test:
  cmp si, 10
  jl body
  ```
- `1704` also reveals **`test reg, imm`** for the
  bit check `if (i & 1)`. Opcode `f7 c6 01 00` =
  `test si, 1`. This sets ZF based on AND result
  *without* modifying SI — cheaper than `and si,
  1 / jz` because the destructive AND would require
  a temp. Then `jz` branches on the result. So
  bit-test patterns lower to:
  ```
  test reg, mask    ; f7 /0 + imm16
  jz / jnz target
  ```
- `1705` (**multi-decl init**): `int a = 1, b = 2,
  c = 3;` produces **byte-identical** code to three
  separate declarations. Each gets its own stack
  slot with its own `mov [m], imm` init. Multi-
  decl is a **parse-time syntactic shortcut** —
  fully expanded into separate declarations before
  codegen.
- `1705` also confirms: locals with only 2 uses
  (init + 1 read) **do NOT enregister**. The
  threshold for enregistration appears to require
  > 2 reads (or reads-across-statements), since
  these 2-use locals stay on stack.

Updated register-allocation rule:
- Enregister a local when **read-count ≥ 2** in
  expressions (NOT counting the init or single
  write). Initial declaration alone doesn't
  trigger enregistration even if it's followed by
  one read.

## 2D array uses `imul` for row stride; goto = unconstrained jmp; `p->field` via `[bx]`

Fixtures `1700` (2D array sum), `1701` (goto loop),
and `1702` (`p->field` arrow operator) round out
the basic control-flow and addressing patterns.

- `1700` (**2D array indexing**): `a[i][j]` (where
  i, j are variables) lowers to:
  ```
  mov ax, si           ; i in SI
  mov dx, 6            ; row-stride bytes (3 ints × 2)
  imul dx              ; ax = i * 6
  mov dx, di           ; j in DI
  shl dx, 1            ; j * 2 (element-size shortcut)
  add ax, dx           ; combined byte offset
  lea dx, [bp-12]      ; base of array
  add ax, dx           ; final pointer offset
  mov bx, ax
  mov ax, [bx]         ; load element
  ```
  Notable: **`imul dx`** for the row stride (since
  the row count is a constant ≥ 2 not a pow2),
  **`shl dx, 1`** for `j * sizeof(int)` (pow2
  shortcut). Mixed strategy based on operand
  characteristics.
- `1700` also uses **`CX` as a third
  enregistered local** (sum accumulator) when SI
  and DI are taken by i, j — confirms the {SI, DI,
  DX, BX, CX} register pool from earlier batches.
- `1701` (**`goto` lowering**): a `goto label`
  emits a **plain `jmp`** to the label's address.
  The `goto loop / goto done` pattern produces
  **byte-identical code to a `while` loop** —
  same `cmp / jl / inc / jmp` structure. The
  compiler treats `goto` as just another control-
  flow primitive; no special analysis.
- `1702` (**`p->field` arrow operator**): lowers
  to `mov bx, [p_ptr] / mov ax, [bx+field_offset]`.
  For `a.next->v` (with `v` at field offset 0),
  the load is just `mov ax, [bx]`. For non-zero
  offset fields, ModR/M with disp8 (`8b 47 disp`)
  or disp16 would be used. The arrow operator is
  **two memory accesses**: load the pointer, then
  deref + field offset in one combined load.

So the small-model addressing toolkit is now
complete:
| Pattern | Encoding |
|---------|----------|
| `[bp+disp]` (local/param) | mod=01/10 rm=110 |
| `[bx]` / `[bx+disp]` (ptr deref) | mod=00/01 rm=111 |
| `[si]` / `[si+disp]` | mod=00/01 rm=100 |
| `[disp16]` (direct global) | mod=00 rm=110 |
| `[bx+si]` etc. (rare) | mod=00 rm=000-011 |

## Recursion via cdecl push+call; SI/DI callee-save; multi-return = one epilogue

Fixtures `1697` (recursive factorial), `1698`
(multi-return sign), and `1699` (3-arg function)
confirm several call-related rules.

- `1697` (**recursive function**): a recursive call
  uses **standard cdecl push+call+pop** — each
  recursion gets its own stack frame. The
  enregistered parameter `n` lives in SI throughout
  one frame. SI is **saved by the callee** via
  `push si` in prologue and `pop si` in epilogue
  (the `5e` byte before `5d c3`). This makes SI/DI
  effectively **callee-preserved**: each function
  that uses them saves and restores them, but the
  caller doesn't need to.

  Confirms SI/DI are callee-save by convention —
  matches the use-count enregistration heuristic
  ([[batch-411-register-allocation]]). Each
  recursion level pushes its own copy.
- `1698` (**multi-return single-epilogue**): a
  function with multiple `return` statements has
  **one epilogue** at the end. Each `return`
  materializes the value in AX and jumps to the
  shared epilogue (`5e 5d c3` or similar). No
  per-return epilogue duplication. The body is:
  ```
  cmp / jcc → return-block-1 → jmp epilogue
  cmp / jcc → return-block-2 → jmp epilogue
  fallthrough return-block-3
  epilogue: pop si / pop bp / ret
  ```
- `1699` (**3-arg cdecl**): args at `[bp+4]`,
  `[bp+6]`, `[bp+8]` in **declaration order**
  (right-to-left push leaves them in this order on
  stack). Caller cleanup uses **`add sp, 6`**
  (3-byte instruction) since 3 args = 6 bytes,
  matching the ≥3-args boundary from
  [[batch-435-arg-cleanup-boundary]]. Below 3
  args, caller uses repeated `pop cx`.

So the cdecl ABI summary is now complete:
- **Args**: pushed right-to-left at 2-byte slots
  starting at `[bp+4]`.
- **Cleanup**: `pop cx` (1 byte each) for 1-2
  args; `add sp, N` (3 bytes) for ≥3 args.
- **Register saves**: SI/DI callee-saved when
  used (push in prologue, pop in epilogue).
- **Return register**: AX (int), DX:AX (long /
  4B struct), ST0 (FP).
- **Hidden args**: large struct return uses
  hidden far-ptr-to-scratch as the
  first push (caller pushes dest then scratch;
  scratch is the callee's "where to write" hint).

## Cross-byte bitfield uses word ops; typedef = no-op; volatile = stack + no CSE

Fixtures `1694` (cross-byte bitfield), `1695`
(typedef int alias), and `1696` (volatile int)
finalise the per-byte-vs-word bitfield rule and
confirm two semantic-only qualifiers.

- `1694` (**cross-byte bitfield**): when a bitfield
  spans a byte boundary, BCC emits **word `81 /N`
  operations** with a 2-byte mask. Layout for
  `a:6, b:6, c:4` (= 16 bits):
  - `a` (bits 0-5, in byte 0): byte ops `80 /N`,
    mask `0xc0` for clear.
  - `b` (bits 6-11, spans bytes 0-1): **word ops**
    `81 /N`, mask `0xf03f` for clear, set value
    `0x0080` (2 << 6).
  - `c` (bits 12-15, in byte 1): byte ops `80 /N`,
    mask `0x0f` for clear (high 4 bits of byte 1).
  - Read of cross-byte `b` uses word load + 6-bit
    shift + 6-bit mask.
  So the rule is **per-field, not per-struct**:
  fields fitting in one byte use byte ops; fields
  crossing use word ops. The clear mask is the
  bitwise-NOT of the field's bit-pattern within
  the storage unit.
- `1695` (**`typedef int`**): produces **byte-
  identical** code to using the underlying type
  directly. `typedef int u16; u16 add(u16, u16)`
  is fully equivalent to `int add(int, int)`.
  `typedef` is a **parse-time alias** with zero
  codegen impact — same OBJ bytes as the
  cdecl-explicit fixture (1656).
- `1696` (**`volatile int`**): forces three things:
  1. Variable lives in **memory** (not enregistered)
     — the use-count heuristic is overridden.
  2. **Every read re-fetches** from memory — no
     CSE, even immediately after a write. The
     `mov ax, [bp-2]` before return executes even
     though we just wrote `[bp-2]` two instructions
     earlier.
  3. **Every write commits** to memory immediately.
  So `volatile` disables enregistration and CSE for
  this variable. Compare to plain `int n = 5; n =
  n + 1; return n;` — n would normally enregister
  into SI/DI with no memory traffic.

For the Rust reimplementation:
- Bitfield codegen: pick byte vs word ops based on
  whether `(bit_offset + bit_width) <= 8` for
  byte-fits, otherwise word ops.
- `typedef`: handle at parser level only, never
  reaches codegen.
- `volatile`: tag the symbol; force memory
  storage; emit load on every read, store on every
  write; bypass register allocation.

## Bitfields: byte-level and/or mask+shift; union shares storage; enum const-folded

Fixtures `1691` (bitfield struct), `1692` (union),
and `1693` (enum constants) characterise three
remaining C semantic shapes.

- `1691` (**bitfield codegen**): a `struct B { a:4;
  b:4; c:8; }` totals 16 bits = 2 bytes. BCC uses
  **byte-level operations** for fields that fit
  within a single byte:
  - **Write `a` (bits 0-3 of byte 0)**: `and byte
    [m], 0xf0` (clear) + `or byte [m], value`
    (set). Uses 4-byte `80 /N` form.
  - **Write `b` (bits 4-7 of byte 0)**: `and byte
    [m], 0x0f` (clear) + `or byte [m], (value <<
    4)` (set). Shifted value at parse time.
  - **Write `c` (whole byte 1)**: `and byte [m+1],
    0x00` (clear) + `or byte [m+1], value` (set).
  - **Read `a`**: `mov al, [m] / and ax, 0x000f`
    (mask).
  - **Read `b`**: `mov dl, [m] / mov cl, 4 / shr
    dx, cl / and dx, 0x000f` (shift+mask).
  - **Read `c`**: `mov dl, [m+1] / and dx, 0x00ff`
    (mask only — no shift needed).

  Cross-byte bitfields would presumably use word
  `81 /N` operations — not yet probed.
- `1692` (**union**): a `union U { int i; char
  c[2]; }` allocates `max(sizeof(members))` = 2
  bytes. Both fields share the same storage —
  `u.i = 0x1234` writes the word; `u.c[0]`
  reads the low byte (= 0x34 since 8086 is little-
  endian). No special codegen — each field is just
  a typed view at offset 0.
- `1693` (**enum constants**): enum values are
  **constant-folded at parse time**. `x + HIGH -
  LOW` where HIGH=20, LOW=5 lowers to `add ax,
  15` directly — no enum symbol in the OBJ, no
  runtime fetch, no separate constant pool. Enums
  are essentially typed `#define` substitutions
  with full constant arithmetic evaluation.

Bitfield encoding policy is a key finding for
Rust reimplementation:
- Track per-field byte offset + bit offset + width.
- Generate `and`/`or` byte ops when field fits in
  one byte; word ops otherwise.
- Shift only when bit-offset within the byte is
  non-zero.
- Mask only when reading (writes use clear+set
  semantics that don't need a read first).

## Struct by value: ≤4B inline pushes, >4B via `N_SPUSH@`; string literal in `_DATA`

Fixtures `1688` (4-byte struct by value), `1689`
(6-byte struct by value), and `1690` (string
literal as arg) close the **struct-passing**
picture.

- `1688` (**small struct by value**): a 4-byte
  struct (2 ints) is **decomposed into 2 word
  pushes** in standard cdecl R-to-L order. Callee
  accesses fields directly at `[bp+4]` (low field)
  and `[bp+6]` (high field). No copy needed — the
  struct IS the stack args. Caller cleans with
  `pop cx; pop cx`.
- `1689` (**large struct by value**): a 6-byte (or
  larger) struct uses a new helper **`N_SPUSH@`**:
  - `DX:AX` = source struct's far address
  - `CX` = byte count
  - Helper pushes the struct bytes onto the
    caller's stack
  Sequence:
  ```
  lea ax, [bp-6]    ; offset of local struct
  mov dx, ss        ; segment (stack)
  mov cx, 6         ; size
  call N_SPUSH@
  call _fn
  add sp, 6         ; caller cleans
  ```
- **Struct-arg boundary**: the rule appears to be
  **≤ 4 bytes → inline word pushes**, **> 4 bytes
  → `N_SPUSH@`**. The 4-byte threshold matches the
  DX:AX return-pair size — structs that fit in the
  return registers are also passed via the cheap
  inline pushes.
- `1690` (**string literal arg**): the literal
  `"ABC"` is placed in `_DATA` as `41 42 43 00`
  (4 bytes with null terminator). At the call site,
  `mov ax, imm16` (FIXUPP-resolved to the data
  segment offset) loads the near pointer, which is
  pushed as a regular word arg. The callee receives
  it at `[bp+4]` as a 2-byte near `char *`.
- `1690` also reconfirms char-iteration loop
  pattern: `mov al, [si] / cbw / add di, ax / inc
  si / cmp byte [si], 0 / jne body`. The `cbw`
  (sign-extend AL→AX) is needed for the int
  promotion of `*s`.

Updated helper table:
| Helper | Purpose | ABI |
|--------|---------|-----|
| `N_SCOPY@` | struct copy | `cx`=count, stack: dst-fp, src-fp; self-cleans 8 |
| `N_SPUSH@` | struct push (call arg) | `dx:ax`=src-fp, `cx`=count; caller cleans byte count |
| `N_FTOL@`  | FP→long              | (no args; FPU ST0) |
| `N_LXMUL@` | long mul             | reg ABI (CX:BX, DX:AX) |
| `N_LDIV@` / `N_LUDIV@` | long div  | stack-passed, self-clean |
| `N_LMOD@` / `N_LUMOD@` | long mod  | stack-passed, self-clean |
| `N_LXLSH@` / `N_LXRSH@` / `N_LXURSH@` | long shifts | reg + CL |
| `FIDRQQ` | FP-lib init marker   | (linker symbol) |
| `FIWRQQ` | FP-word-return marker | (linker symbol) |

Both `N_SCOPY@` and `N_SPUSH@` cement the
"compiler emits a `mov cx, size / call helper`
pattern for struct value operations larger than a
register pair" boundary.

## Large struct return: 2-stage copy via `N_SCOPY@` + scratch; static local = BSS

Fixtures `1685` (3-int struct return), `1686`
(static local in fn), and `1687` (if/else with fn
calls) close several open questions.

- `1685` (**large struct return ABI**): structs
  larger than 4 bytes use a **2-stage copy** via
  `N_SCOPY@`:
  1. **Caller** pushes far ptr to **final
     destination** (4 bytes, seg+off).
  2. **Caller** pushes far ptr to **scratch
     buffer** (4 bytes) — a local on caller's
     stack.
  3. Caller calls the function.
  4. **Callee** builds the struct locally, then
     copies its local → scratch via `N_SCOPY@`
     (using the scratch ptr from `[bp+4..7]`).
  5. **Callee** returns the dest offset in AX (per
     convention), with the scratch already
     populated.
  6. Caller `pop cx; pop cx` (cleans the scratch
     ptr only — the dest ptr remains on stack).
  7. Caller pushes scratch ptr again (as **source**
     for N_SCOPY@).
  8. Caller calls `N_SCOPY@` with `cx = byte
     count`, dest still on stack from step 1,
     source just pushed.
  9. `N_SCOPY@` self-cleans 8 bytes.

  So **two `N_SCOPY@` calls per struct-return
  expression** (one inside callee, one in caller).
  The 2-stage copy lets callee's local lifetime end
  cleanly before the value lands in the final dest
  — important when the call result feeds into a
  larger expression (e.g., `g(mk())`).
- `1685` also reveals new helper: **`N_SCOPY@`**
  (struct copy) with ABI:
  - Stack: dest far ptr (high), src far ptr (low)
  - `CX`: byte count
  - Self-cleans 8 bytes of stack args
- `1686` (**static local**): a `static int n = 0;`
  inside a function lives in **`_BSS`** (BSS-
  zero-initialised since the initializer is 0).
  Access is via direct memory `ff 06 [_n]` (inc
  word [mem]) and `a1 [_n]` (mov AX, mem) — same
  as a file-scope global. **No first-use init
  guard** because BSS already zero-initialises at
  program startup. If the static had a non-zero
  initializer, it would be in `_DATA` instead.
- `1687` (**if-else with calls**): standard branch
  pattern — `or si, si / jle L_else / call f / jmp
  L_done / call g`. The result variable `r` gets
  enregistered into DI since it's used after both
  branches.

Open probes:
- Pascal calling convention with struct return
  (likely uses `ret imm16` for cleanup of hidden
  ptr args).
- struct passed by value as parameter (the
  inverse of struct return).

## Return ABIs: int in AX, long in DX:AX, 4-byte struct in DX:AX, double on ST0

Fixtures `1682` (2-int struct return), `1683` (long
return), and `1684` (double return) characterise the
function-return ABI by type:

- **int**: returned in AX (the standard for the 8086
  cdecl).
- **long** (`1683`): returned in **DX:AX** (high
  half in DX, low half in AX). The same register
  pair used by `N_LXMUL@` and other long helpers.
- **Small struct ≤ 2 words** (`1682`): also
  returned in **DX:AX**! For a `struct { int x;
  int y; }`, the function loads `mov ax, [y_field]`
  / `mov dx, [x_field]` from the local instance and
  the caller stores both back to its receiving
  variable via `mov [r_high], dx / mov [r_low],
  ax`. So 4-byte structs share the long ABI.
- **double** (`1684`): returned on the **FPU stack
  top (ST0)**. Callee leaves `fld qword [literal]`
  hanging on the FPU stack, then `ret`s with the
  value still there. The caller immediately does
  `fstp qword [dest]` after the call to capture it.
  Zero memory traffic for the return value crossing
  the call boundary — efficient.

So the return ABI summary:
| Return type | Mechanism |
|-------------|-----------|
| `char` / `short` / `int` / near `*` | AX |
| `long` / `unsigned long` | DX:AX |
| `far *` | DX:AX (offset in AX, segment in DX) |
| 1-2 word struct | DX:AX |
| 3+ word struct | (not yet probed; likely hidden ptr arg or static buffer) |
| `float` / `double` | ST0 (FPU stack top) |
| `void` | nothing (AX may be clobbered) |

For the Rust reimplementation:
- The callee emits stores to AX (or DX:AX, or ST0) just before the `ret`.
- The caller knows the return type and emits the
  corresponding capture immediately after the call:
  - `mov [r], ax` (int)
  - `mov [r_high], dx / mov [r_low], ax` (long)
  - `fstp qword [r]` (double)

Large-struct return is an open probe.

## Float array stride 4; global double full 8-byte storage; `fdiv` native

Fixtures `1679` (float array), `1680` (double
global with init 3.14), and `1681` (double division)
all pass on the first capture.

- `1679`: a `float a[3]` on the stack lays elements
  at 4-byte stride (`[bp-12]`, `[bp-8]`, `[bp-4]`).
  Sum chain `a[0]+a[1]+a[2]` runs entirely on the
  FPU stack without intermediate spills:
  ```
  fld [a[0]]
  fadd [a[1]]
  fadd [a[2]]
  ```
  The FPU's deeper register stack (8 slots)
  accommodates these in-flight results — no need
  to materialise intermediates to memory. Also
  reconfirms **`fld1`** for the `1.0f` literal in
  array element init.
- `1680` (**global double full-precision**): a
  global `double g = 3.14;` is stored in `_DATA` as
  **8 bytes full double precision** — `1f 85 eb 51
  b8 1e 09 40` (3.14 exactly as IEEE 754 double).
  Unlike the local-literal optimisation in [[batch-
  453-fp-conv]] which can downconvert to float
  storage, globals must preserve the declared type
  exactly. The load is `fld qword [_g]` (`9b dd /0`).
- `1681` (**FP division native**): `double / double`
  uses **`fdiv qword [m]`** (`dc /6`, ModR/M `76` =
  mod=01 rm=110 [bp+d] /6=FDIV) directly. No
  helper call — the FPU does all FP arithmetic
  natively (add/sub/mul/div). Only the int
  conversion needs `N_FTOL@`.

Updated FP op encoding additions:
| Op | Encoding |
|----|----------|
| `fdiv qword [m]`  | `9b dc /6` |
| `fdiv dword [m]`  | `9b d8 /6` |
| `fdivr qword [m]` | `9b dc /7` (reverse) |
| `fdivr dword [m]` | `9b d8 /7` |

So FP arithmetic is **entirely inline** — only `<-> int` conversion uses helpers. The Borland FP support is mostly just instruction emission with the 8087/8088 op set.

## FP conv free via reg stack; FP arg = `sub sp / fstp qword [sp]`

Fixtures `1676` (float→double), `1677` (double→
float), and `1678` (function taking double param)
all pass on the first capture.

- `1676`/`1677`: **FP↔FP conversions are free** via
  the FPU register stack. `float → double` is
  `fld dword [f] / fstp qword [d]` — the FPU
  internally operates at 80-bit extended precision,
  so the precision conversion happens automatically
  on load/store. No helper, no special opcodes —
  just `fld <src-prec> / fstp <dst-prec>`.
- `1678` (**FP argument passing**): a `double`
  parameter is passed on the stack as 8 bytes, but
  the push protocol differs from int args:
  ```
  fld dword [literal]      ; load source value
  sub sp, 8                ; reserve 8 bytes for double arg
  fstp qword [bp-8]        ; store-and-pop into reserved slot
  nop ; wait               ; FPU sync
  call _fn
  add sp, 8                ; caller cleans (cdecl)
  ```
  So FP args use **`sub sp, N / fstp [sp]`** instead
  of `push imm` chains. Cleanup uses **`add sp, 8`**
  per 8-byte double (or `add sp, 4` per float).
- **Double literal storage optimisation**: a `3.5`
  (source-level double) is stored in `_DATA` as a
  **4-byte float** when it round-trips losslessly
  through float precision. The call site loads as
  float and promotes to double via the FPU stack.
  Saves 4 bytes per literal when applicable. BCC
  checks the value at parse time and picks the
  smaller storage form.
- Callee accesses double param at `[bp+4]` as
  qword: `9b dd 46 04 = fld qword [bp+4]`. Same
  offset as a near pointer arg — the param's
  bytes are just 8 wide instead of 2.

So FP argument passing in cdecl uses a different
push mechanism (FPU-store rather than CPU push)
but otherwise follows the same stack discipline:
caller-cleans, args at `[bp+4]+`, right-to-left
order (not yet tested with multiple FP args).

## FP `1.0` via `fld1`; FP cmp uses `fstsw`+`sahf`; `int→float` via `fild`

Fixtures `1673` (`a*b - 1.0f`), `1674` (FP `<` cmp),
and `1675` (`(float)int`) reveal more FP codegen
details:

- `1673`: **constant `1.0` uses `fld1`** (opcode
  `d9 e8`, 2 bytes) instead of `fld dword [literal]`
  (5 bytes + FIXUPP). BCC recognises specific FP
  constants and uses the FPU's load-constant
  instructions:
  - `fld1` (`d9 e8`) — load 1.0
  - `fldz` (`d9 ee`) — load 0.0 (not yet probed)
  - `fldpi`, `fldl2e`, etc. — other constants
  Saves both code bytes and a data slot.
- Also from `1673`: **`fmul dword [m]`** (`d8 /1`)
  for FP mul-mem; **`fsubp ST(1)`** (`de e9`) for
  FP subtract-pop. So FP arithmetic has memory-
  operand variants (`d8 /N`) and stack-popping
  variants (`de /N`).
- `1674` (**FP comparison ABI**): FP `<` lowers to:
  ```
  fld a
  fcomp dword [b]         ; d8 /3, sets FPU status flags
  fstsw word [bp+disp]    ; dd /7, save status word to mem
  mov ax, [mem]           ; load status word
  sahf                    ; 9e — copy AH to CPU flags
  jae L_false             ; unsigned branch (FPU maps to above/below)
  ```
  The FPU status word's bits C3/C2/C0 map to ZF/PF/
  CF when transferred via `sahf`. So FP compares
  always use **unsigned-flavour jcc** (`jae`/`jb`/
  etc.) regardless of source-level operator.
- `1674` also reveals a new external: **`FIWRQQ`**
  — Borland's word-return FP marker, emitted
  whenever the program uses FP and produces a
  word-sized return value.
- `1675` (**int→float**): **`fild word [bp+disp]`**
  (`df /0`) loads a word integer and auto-converts
  to FP. No helper call needed — the FPU does the
  conversion natively. So:
  - `int → float`: just `fild` (1 instr, native)
  - `float → int`: call **`N_FTOL@`** (helper-
    based since 8086 has no native FP→int with
    truncation that matches C semantics)

Updated FP codegen catalogue:
| Op | Encoding |
|----|----------|
| `fld dword [m]`  | `9b d9 /0` |
| `fld qword [m]`  | `9b dd /0` |
| `fld1`           | `9b d9 e8` |
| `fild word [m]`  | `9b df /0` |
| `fstp dword [m]` | `9b d9 /3` |
| `fstp qword [m]` | `9b dd /3` |
| `fadd dword [m]` | `9b d8 /0` |
| `fmul dword [m]` | `9b d8 /1` |
| `fsubp ST(1)`    | `9b de e9` |
| `fcomp dword [m]`| `9b d8 /3` |
| `fstsw word [m]` | `9b dd /7` |
| `sahf`           | `9e` (no wait) |

## Floating-point: 8087 FPU instructions with `9b` wait prefix; `FIDRQQ` + `N_FTOL@`

Fixtures `1670` (`float f = 3.0f`), `1671` (`float
a+b`), and `1672` (`double d = 3.0`) reveal the
floating-point codegen — **completely separate
toolchain** from integer code:

- **FPU instructions used**: all FP ops are
  Borland's 8087 FPU code-emission with each
  instruction **prefixed with `0x9b` (WAIT)** for
  CPU/FPU synchronisation on early machines. So
  `9b d9 06 disp16` = `wait ; fld dword [m]`,
  `9b d9 5e disp8` = `wait ; fstp dword [bp+d]`,
  etc. The wait prefix is *always* emitted before
  each FP instruction.
- **EXTDEF `FIDRQQ`**: Borland's runtime emits this
  magic external — the linker pulls in the FP
  library if this symbol is referenced. Any TU
  using float/double generates this.
- **EXTDEF `N_FTOL@`**: float/double → long helper.
  Called when casting `(int)f` or `(long)f`:
  - FPU loads the value onto ST0
  - call N_FTOL@
  - Returns DX:AX (the 32-bit long, narrowed to int
    by taking AX only)
- **Float (4 bytes) vs double (8 bytes)**:
  - Float ops: `9b d9 /N` (load/store dword,
    arithmetic with dword mem operand uses `9b d8
    /N`)
  - Double ops: `9b dd /N` (load/store qword)
  - The opcode group selects precision; the ModR/M
    /N selects the operation.
- **Float add** (`a + b`): `9b d9 06 [a]` (fld a),
  `9b d8 06 [b]` (fadd b), `9b d9 5e [r]` (fstp r).
  No CSE — load a, add b, store result; standard
  three-instruction FP binop.
- **Literal floats**: stored in `_DATA` as IEEE 754
  little-endian — e.g. `3.0f` = `00 00 40 40` (4
  bytes), `3.0` (double) = `00 00 00 00 00 00 08
  40` (8 bytes).

So FP support is a **distinct codegen path** that
needs its own implementation in the Rust
reimplementation:
- FPU instruction encoder (with `0x9b` prefix
  injection)
- IEEE 754 constant encoding in `_DATA`
- Plumbing for `FIDRQQ` (always emit when FP
  detected) and `N_FTOL@` (emit when narrowing FP
  to int/long)
- Other helpers likely exist (e.g., long→float,
  printf-fp-format) — not yet probed.

## Large model: `int *` is far automatically; stack arr & enregistration unchanged

Fixtures `1667` (`int *p = &g; *p = 99;`), `1668`
(stack int array), and `1669` (multi-use locals
with longhand assign) extend the large-model
exploration.

- `1667` (**`int *` is auto-far**): in large model,
  `int *p` is automatically a **4-byte far
  pointer**, even without an explicit `far`
  qualifier. `&g` produces a `ds:offset` far
  address — the code emits `mov [bp-2], ds`
  (segment capture via `0x8c /3` mod=11 = mov r/m,
  DS) and `mov [bp-4], 0` (offset, FIXUPP'd). The
  deref-write `*p = 99` uses the standard `les bx,
  [p] / mov es:[bx], 99` path with the `0x26` ES
  prefix. So the small-model `int *p` shape (2-byte
  near ptr, direct `[si]` deref) is replaced with
  the far-ptr shape that was previously seen only
  under the explicit `far` qualifier.
- `1668` (**stack arrays unchanged**): a `int a[3]`
  on the stack with constant indices generates
  **identical code** to small model — `mov [bp+disp],
  imm` for each store, `mov ax, [bp+disp]` for the
  return. Stack-resident data implicitly uses SS, so
  no far-pointer machinery is needed. Only the
  epilogue byte differs (`5d cb` vs `5d c3`).
- `1669` (**register allocation unchanged**): multi-
  use ints still enregister into SI/DI via the same
  use-count heuristic. The `a = a + 1` longhand
  still uses the **AX round-trip** (`mov ax,si /
  inc ax / mov si,ax`) — same as small model's
  fixture `1568`. So **IR-level rules port
  identically** across models. Only the epilogue
  byte (`cb` vs `c3`) reflects the model.

Cross-model rule summary for the code-generation
encoder:
- **IR-level**: register alloc, encoding policies,
  inc/dec optimization, narrow-cast propagation,
  loop normalisation, switch dispatch — **all port
  identically**.
- **ABI-level**: only the call ABI (push cs +
  near vs near alone) and epilogue (`retf` vs
  `ret`) change.
- **Type-level**: `int *` width and codegen path
  change based on data model:
  - Near data: 2-byte ptr, `[si]` deref
  - Far data: 4-byte ptr, `les + 26` deref
- **OBJ structure**: segment naming
  (`_TEXT` → `<MODULE>_TEXT`).

So the multi-model story is **largely orthogonal**
to the deep encoding findings — the encoder needs a
small number of model-conditional emissions.

## Large model (-ml) initial probe: HELLO_TEXT, retf everywhere, push cs + call near

Fixtures `1664` (trivial return-zero), `1665` (call
inc(5)), and `1666` (global int access) are the
**first batch captured under `-ml` (large model)**.
All pass on the first capture. Cross-model
differences vs small (-ms):

- **Code segment name**: `_TEXT` → `HELLO_TEXT`. The
  large model gives each translation unit its own
  uniquely-named code segment, prefixed with the
  module name (uppercased). The SEGDEF and LNAMES
  records reflect this — `_TEXT` is replaced
  throughout. This means we'll need different
  string handling for the segment-name fields per
  model.
- **Function return**: every function uses **`retf`
  (`0xcb`)** instead of near `ret` (`0xc3`). In
  large model, *all* functions are far by default,
  matching the explicit `far` qualifier in small
  model ([[batch-445-pascal-far-fn]]).
- **Function call sites**: intra-module calls use
  **`push cs / call near (e8)`** (4 bytes) instead
  of `call near` alone (3 bytes). The `push cs`
  (`0x0E`) is the standard 1-byte trick that lets
  the callee's `retf` pop both seg+off correctly.
- **Globals**: still accessed via DS-relative
  `a1 disp16` / `a3 disp16` — same as small model.
  Borland's runtime startup sets DS=DGROUP, and as
  long as the program doesn't change DS, globals
  work the same way. The far-data-ness of large
  model shows up only when **`&global`** is taken
  (producing a 4-byte far pointer; not probed yet).
- **Module-flag byte**: at OBJ offset ~0x4d, small
  uses `7f`, large uses `7c`. Different bits in the
  COMENT record's class byte indicating model.

So the multi-model story:
- The IR-level findings (register allocation,
  inc/dec, encoding policies, loop normalisation,
  switch dispatch, etc.) remain unchanged.
- What differs is **fixed prefixes per call**
  (`push cs`), **epilogue bytes** (`retf` not
  `ret`), **segment-name strings**, and
  **module-flag bytes**.

For the Rust reimplementation:
- Plumb a `memory_model` parameter from
  `invocation.toml` (or a new `model` field) down
  to the OBJ emitter.
- Conditionally inject `push cs` before each
  intra-module call when code is far.
- Emit `retf` (`0xcb`) instead of `ret` (`0xc3`)
  for all function epilogues when code is far.
- Use the module-prefixed segment name in SEGDEF /
  LNAMES.

## `return a<b`; `(5,7)` drops LHS; `while(n)` uses `or si,si`

Fixtures `1661` (`return a < b;` direct return),
`1662` (`int x = (5, 7);` comma op in init), and
`1663` (`while (n)` truthiness) all pass on the
first capture.

- `1661`: a comparison result returned directly uses
  the **same boolean materialisation template** as
  if it were assigned to a variable. No "direct
  return" optimisation — `cmp / inv-jcc / mov ax,
  1 / jmp / xor ax,ax` always runs, and the result
  in AX is just used as the return register.
- `1662`: comma operator `(5, 7)` with constant LHS
  **drops the LHS entirely** at compile time. Only
  `mov [bp-2], 7` is emitted for `int x = (5, 7)`.
  Constant sub-expressions with no side effects in
  comma's left operand are discarded by the parser/
  AST. If the LHS had side effects (function call,
  assignment), it would have to be emitted —
  worth a future probe.
- `1663` (`while (n)`): standard bottom-test loop
  with **`or si, si / jne body`** as the truthiness
  test on register-resident `n`. Confirms the
  zero-test shortcut for enregistered locals from
  [[batch-414-cmp-zero-or-reg]] / fixture `1560`
  works in loop condition context too.

These three are all confirmations of previously-
identified patterns applied in slightly different
contexts. Useful for cross-checking but no new
findings.

## Indirect call via `ff /2`; `n--` returns old via `dec [mem]`

Fixtures `1658` (fn-ptr array indirect call), `1659`
(`while (decr())` fn-call cond), and `1660` (array
stores from binops) all pass on the first capture.

- `1658` (**indirect near call**): calling through
  a function pointer uses **`ff /2` (call near
  r/m16)** — specifically `ff 56 disp` for `call
  word [bp+disp]`. Same opcode family as data access
  (`ff` with /2 ModR/M selects "call indirect" vs
  /0 for inc, /1 dec, etc.). For an array of fn
  pointers, each call site emits `ff 56 disp` with
  the appropriate offset.
- `1659` (**`n--` global**): returning post-
  decrement of a global uses **`a1 [_n]`** (load
  AX from global) followed by **`ff 0e disp`** —
  `dec word [_n]` (opcode `ff /1` mod=00 rm=110
  with disp16 = direct memory). So `return n--`
  is a two-instruction post-decrement: load
  pre-value into return register, then `dec word
  [mem]` in place. No temp save needed since the
  return value was captured before the dec.
- `1660`: array stores from binops use the small-
  expression shortcuts in expression context:
  `a[2] = x - 1` lowers to `mov ax,si / dec ax /
  mov [bp-2], ax`. So `expr - 1` does use `dec
  ax` (1 byte) even in expression context — the
  longhand `i = i - 1` AX-roundtrip
  ([[batch-417-inc-dec-syntactic-split]]) was
  specific to the assignment IR shape.

The `ff /N` opcode family is now characterised:
| /N | Op             | Notes |
|----|----------------|-------|
| /0 | `inc r/m16`    | (used for memory inc) |
| /1 | `dec r/m16`    | (used for memory dec like `n--`) |
| /2 | `call near r/m16` | (indirect call) |
| /3 | `call far ptr16` | (far indirect) |
| /4 | `jmp near r/m16` | (computed jump — switch table) |
| /5 | `jmp far ptr16` | |
| /6 | `push r/m16`   | |

## `interrupt` saves all regs + sets DS; `cdecl` explicit; `x - K` via `add ax, -K`

Fixtures `1655` (`interrupt void isr()`), `1656`
(`int cdecl add`), and `1657` (3-deep call chain
with subtract) all pass on the first capture.

- `1655` (**interrupt function**): emits a massive
  9-register save prologue: `push ax / push bx /
  push cx / push dx / push es / push ds / push si
  / push di / push bp`. Then **re-establishes DS to
  DGROUP** via `mov bp, DGROUP / mov ds, bp` (the
  `bd disp` is FIXUPP'd to DGROUP). Body runs.
  Epilogue pops everything in reverse and ends with
  **`iret`** (opcode `0xCF`, 1 byte) — interrupt-
  return that pops both flags and CS:IP. Classic
  8086 ISR pattern.
- `1656` (**explicit `cdecl`**): produces byte-
  identical code to the default convention —
  underscore-prefixed symbol `_add`, args at
  `[bp+4]+` in declaration order (right-to-left
  push), caller cleans with `pop cx; pop cx`. So
  `cdecl` is a no-op qualifier in BCC's default
  small-model setup.
- `1657` (**`g(x) - 3`** encoding): subtract of a
  constant `g(x) - 3` lowers to **`add ax, -3`**
  (`05 fd ff` — opcode `0x05` add-AX-imm16 with
  imm16 = 0xFFFD = -3 two's complement). BCC
  canonicalises `x - K` (positive K) as `x + (-K)`
  using the AX-with-imm ADD opcode, NOT as `sub ax,
  K`. So subtract-of-positive-imm-from-AX uses ADD
  with negative imm.

These complete the calling-convention picture for
the small model. The `interrupt` lowering will look
the same in larger memory models — just with `iret`
unchanged (always 1 byte). Multi-memory-model
divergence will mostly affect the `near` vs `far`
call sequences and pointer sizes (already
characterised in [[batch-444-far-pointers]] and
[[batch-445-pascal-far-fn]]).

## `huge` = far in deref; `pascal` callee-cleans+uppercase; `far` fn `push cs; call`

Fixtures `1652` (`int huge *p`), `1653` (`pascal`
calling convention), and `1654` (`far` function) all
pass on the first capture and reveal three more
Borland extension codegen patterns.

- `1652`: **`huge` and `far` produce byte-identical
  code for simple deref** — both store 4 bytes
  (seg:off), use `les` + `26` ES override. The
  difference only shows up in pointer arithmetic
  across segment boundaries (huge would normalise,
  far wouldn't). Simple `*p` cases are
  indistinguishable.
- `1653` (**pascal calling convention**):
  - **PUBDEF symbol is `ADD`** (uppercase, no
    underscore prefix) — pascal name mangling
    strips the C `_` and uppercases.
  - **Args pushed left-to-right** (instead of
    cdecl's right-to-left). Callee accesses first
    arg at `[bp+6]` (pushed first → higher offset),
    second arg at `[bp+4]`.
  - **Callee cleans args via `ret imm16`** (opcode
    `c2 04 00` for 4-byte cleanup). No caller post-
    call `pop cx` / `add sp, N`. Saves bytes per
    call site at cost of 3 bytes per function.
- `1654` (**far function**):
  - Callee uses **`retf`** (opcode `0xCB`, 1 byte)
    instead of near `ret`.
  - Args accessed at **`[bp+6]+`** because the far
    return address occupies 4 bytes (seg:off)
    instead of 2.
  - **Caller emits `push cs ; call near`** (4 bytes)
    instead of `call far ptr16:16` (5 bytes) when
    calling within the same segment. The `push cs`
    (opcode `0x0E`) pushes the return segment so
    the callee's `retf` pops both seg+off correctly.

These Borland extensions complete the basic
calling-convention picture: cdecl (caller-cleans,
underscore prefix, right-to-left), pascal (callee-
cleans, UPPERCASE, left-to-right), and far/near
distinguish near `ret` vs `retf` based on the call
distance. Mixing models would generate `call far`
(`9A` opcode) explicitly.

## `far` pointers: 32-bit seg:off, `les` + `26` ES override

Fixture `1649` (`int far *p = (int far *)&x; return
*p;`) compiles cleanly in small model and reveals
the **far-pointer codegen** model:

- A `far` pointer is **32 bits** (2 words on stack):
  - Lower word: offset
  - Higher word: segment
- Constructing a far pointer from a near address
  uses **`mov [seg_slot], ss`** (opcode `8c /2`,
  `mov r/m16, SS`) to capture the local's segment
  (which is `SS` for stack-allocated `x`), then a
  `mov [off_slot], ax` for the lea'd offset.
- Loading the far pointer for deref uses **`les bx,
  [bp+disp]`** (opcode `c4 /r`, "Load far pointer
  into ES:reg") — single instruction loads both
  offset into BX and segment into ES from the 4-byte
  source.
- The actual memory access through the far pointer
  uses an **`ES:` segment-override prefix** (byte
  `0x26`): `26 8b 07` = `mov ax, es:[bx]`.

So the lowering pattern for `int far *p; *p = ...`:
```
8c 56 disp   ; mov [p_seg], ss          (or other seg)
89 46 disp   ; mov [p_off], ax
c4 5e disp   ; les bx, [p]
26 8b 07     ; mov ax, es:[bx]          (or write equivalent)
```

For the Rust reimplementation: the far-pointer
support is a known Borland extension — needs:
- Recognising `far`/`near`/`huge` type qualifiers in
  the parser.
- Treating `int far *` as a 32-bit type (4 bytes,
  word-aligned with high half = segment).
- Emitting `8c /2` for segment captures, `c4 /r`
  for far-pointer loads, and `26` prefixes for ES-
  based memory access.
- Stack locals' addresses naturally have `SS` as
  segment; global/static would use `DS` (i.e. `8c
  56 disp` for stack and `8c 5e disp` for DS-based
  globals).

## `(int)(long+long)` skips high; long arg cdecl; long cmp folds int const

Fixtures `1646` (`long a[2]; (int)(a[0]+a[1])`),
`1647` (`long sqr(long x)` with long arg+return),
and `1648` (`long < int_const`) all pass on the
first capture.

- `1646` (**narrow-cast on long add**): `(int)(long
  + long)` discards the high-half computation just
  like `(char)(int + int)` discards the high byte.
  Code emits only `mov ax, [a_low] / add ax, [b_low]`
  — **no `adc` on the high halves** since they would
  be cast away. So BCC's narrow-cast propagation
  pass works at the long-word level too, not just
  the int-byte level.
- `1647` (**long parameter passing**): a `long`
  argument is passed as **two consecutive word
  pushes**, with the **high half pushed first**
  (lands at higher offset). Inside the callee:
  - `[bp+4]` = low word
  - `[bp+6]` = high word
  Long return is via `DX:AX` register pair. After
  the call site, **`pop cx; pop cx`** cleans 4 bytes
  (matches the 2-arg cleanup rule from
  [[batch-435-arg-cleanup-boundary]] — long counts
  as 2 word-args worth of cleanup).
- `1648` (**long cmp against int const**): the int
  constant is **promoted to long at compile time**
  — the cmp uses `cmp [bp+disp], 0` for the high
  half (since int 10 has high=0) and `cmp [bp+disp],
  10` for the low half. Both use the `83 /7` imm8-
  sext encoding. So mixed long-vs-int-const cmp is
  pre-folded at parse time, then the standard
  inline two-step long compare runs.

These three fixtures complete the long-type picture
for codegen: aggregates, parameter passing, and
type-promoted comparisons all work as expected.

## `int + long`: int gets `cwd`-promoted; long `==` two-step; long `&` two-word

Fixtures `1643` (`int + long`), `1644` (`long ==
long`), and `1645` (`long & long`) extend the long
arithmetic picture.

- `1643` (**mixed `int + long`**): the int operand
  is promoted to long via **`cwd`** first, then
  standard inline long add (add+adc). Sequence:
  `mov ax,[i] / cwd / add ax,[b_low] / adc dx,
  [b_high] / store r`. So C usual arithmetic
  conversion (UAC) is applied at IR level —
  int→long widening via cwd, then mixed-type
  expression runs at long width.
- `1644`: `long == long` is **inline** like `<`
  but simpler — both `cmp` use `jne` to bail to
  false. No signed/unsigned distinction needed
  since equality is bit-pattern:
  ```
  cmp ax, [b_high]   ; high cmp
  jne false
  cmp dx, [b_low]    ; low cmp
  jne false
  ; true path
  mov ax, 1 / jmp / xor ax,ax
  ```
- `1645`: `long & long` is **inline** two-word:
  `and dx, [b_low] / and ax, [b_high] / store`. No
  carry needed for bitwise ops. Same shape applies
  to `|` and `^`.

So the inline-vs-helper boundary for long ops:

| Op | Inline | Helper |
|----|--------|--------|
| `+`, `-`     | yes (add+adc, sub+sbb) | — |
| `&`, `|`, `^`| yes (two-word)         | — |
| `==`, `!=`   | yes (two-step cmp)     | — |
| `<`, `>`, `<=`, `>=` | yes (high signed, low unsigned) | — |
| `*`          | —      | `N_LXMUL@` |
| `/`, `%`     | —      | `N_L[U]DIV@` / `N_L[U]MOD@` |
| `<<`, `>>`   | —      | `N_LX[U]LSH@` / `N_LXR[S]H@` |

The boundary: arithmetic that requires multi-step
loops (mul/div) or multi-bit shifts goes to helpers;
single-pass two-word ops are inlined.

## Long const shift still calls helper; `long * pow2` becomes shl helper

Fixtures `1640` (`long >> 4` with constant shift),
`1641` (`long * 4L` const pow2 multiply), and `1642`
(`int i; long r = i + 1`) all pass on the first
capture.

- `1640`: even a **constant** long shift count still
  calls **`N_LXRSH@`** (not inlined). Code emits
  `mov cl, 4 / call N_LXRSH@`. So no shift-by-1
  unrolling for longs — the helper is invoked for
  any shift amount.
- `1641`: `long * 4L` (pow2) is recognised and
  lowered to **`N_LXLSH@` with cl=2** (log2 of 4) —
  the same mul-by-pow2 → shift optimisation applies
  to longs as to ints, but the shift itself goes
  through the helper. So no `N_LXMUL@` call here.
- `1642` (**C promotion rule confirmed**): `int + 1`
  is computed at **int width first**: `mov ax,
  [bp-2] / inc ax`. Only then is the int result
  widened to long via `cwd` for the assignment to
  `long r`. So integer-typed sub-expressions stay
  int-width even when the result is assigned to a
  long. Standard C type-promotion: only operands of
  identical "rank" are operated on at their
  common type; mixed-rank promotes the lower-rank
  to the higher.

This means long arithmetic only kicks in when both
operands are long. `int + long` would (per C rules)
promote the int to long first, then use long
operations. Not yet probed.

For the Rust reimplementation:
- Don't inline long shifts even for constant counts.
- Recognise `long * pow2` and convert to `<< log2`
  before codegen (so the shift helper is used).
- Implement C usual arithmetic conversions in the
  IR.

## `N_LUMOD@`; int→long via `cwd`; uint→long zero-fills high

Fixtures `1637` (`unsigned long % unsigned long`),
`1638` (`(long)signed_int`), and `1639`
(`(long)unsigned_int`) complete the long-helper
picture and characterise integer-to-long widening.

- `1637` (**N_LUMOD@**): unsigned long mod has its
  own helper, distinct from signed `N_LMOD@`. Same
  stack-passed self-cleaning ABI.
- `1638` (**signed int → long via `cwd`**): the
  widening lowers to `mov ax, [int] / cwd / mov
  [high], dx / mov [low], ax`. The `cwd` (`0x99`)
  is a 1-byte sign-extend instruction that fills DX
  with copies of AX's MSB — exactly what's needed
  for signed widening (DX = 0xFFFF if negative,
  0x0000 if non-negative).
- `1639` (**unsigned int → long zero-fills**): the
  widening lowers to `mov ax, [uint] / mov word
  [high], 0 / mov [low], ax`. The high half is
  explicitly zeroed with a 5-byte
  `c7 46 disp 00 00` (`mov [bp+disp], imm16`)
  rather than via `cwd` (which would sign-extend
  and produce the wrong result for values with the
  high bit set).

So the integer→long widening choice is **signedness-
driven at the source-type level**:
- `int` (signed) → long: `cwd` (1 byte to fill high)
- `unsigned int` → long: `mov word [high], 0` (5
  bytes)

Final long-helper table now complete for arithmetic:
| Helper | Op | ABI |
|--------|-----|-----|
| `N_LXMUL@`  | `long *`           | reg |
| `N_LDIV@`   | signed `/`         | stack, self-clean |
| `N_LUDIV@`  | unsigned `/`       | stack, self-clean |
| `N_LMOD@`   | signed `%`         | stack, self-clean |
| `N_LUMOD@`  | unsigned `%`       | stack, self-clean |
| `N_LXLSH@`  | `<<`               | reg + CL |
| `N_LXRSH@`  | signed `>>`        | reg + CL |
| `N_LXURSH@` | unsigned `>>`      | reg + CL |
| `(inline)`  | add/sub/and/or/xor | with carry chains |
| `(inline)`  | comparisons         | hi/lo two-step |
| `(inline)`  | int→long           | `cwd` or zero-fill |

## Long mod `N_LMOD@`, unsigned shr `N_LXURSH@`, add INLINED `adc`

Fixtures `1634` (`unsigned long >> int`), `1635`
(`long % long`), and `1636` (`long + long`) extend
the long-arith picture and reveal a key principle:
**only the non-trivial ops use helpers**.

- `1634` (**N_LXURSH@**): unsigned long shr uses
  this distinct helper (vs signed `N_LXRSH@`).
  Same DX:AX + CL register ABI.
- `1635` (**N_LMOD@**): long mod uses its own
  helper. Stack-passed ABI, self-clean, returns
  remainder in DX:AX.
- `1636` (**inline long add**): no helper. Lowers to
  ```
  mov ax, [a_high] / mov dx, [a_low]
  add dx, [b_low]      ; low halves
  adc ax, [b_high]     ; carry-propagating high halves
  mov [r_high], ax / mov [r_low], dx
  ```
  Uses the 8086 `adc` instruction (opcode `0x13`)
  to propagate carry from low to high. So **long
  add/sub/and/or/xor are all inlined** with the
  appropriate carry-propagating two-word sequence:
  - `add` + `adc`
  - `sub` + `sbb`
  - `and` + `and` (no carry needed)
  - `or`  + `or`
  - `xor` + `xor`

**Final long-helper table**:
| Helper | Op | ABI |
|--------|-----|-----|
| `N_LXMUL@`  | `long *`          | reg DX:AX,CX:BX |
| `N_LDIV@`   | signed `/`        | stack, self-clean |
| `N_LUDIV@`  | unsigned `/`      | stack, self-clean |
| `N_LMOD@`   | signed `%`        | stack, self-clean |
| `N_LXLSH@`  | `<<`              | reg DX:AX, CL |
| `N_LXRSH@`  | signed `>>`       | reg DX:AX, CL |
| `N_LXURSH@` | unsigned `>>`     | reg DX:AX, CL |
| **inline**  | `+`,`-`,`&`,`|`,`^` | two-word add+adc etc. |
| **inline**  | comparisons       | high-then-low cmp |

Still to probe: `N_LUMOD@` (unsigned mod), long
conversions (int↔long, char↔long), long shift by
constant (likely still helper since count comes via
CL).

## Long shl/cmp/udiv: `N_LXLSH@`, inline cmp, `N_LUDIV@`

Fixtures `1631` (`long << var`), `1632` (`long <
long` with two long operands), and `1633` (unsigned
`long / long`) extend the long-arithmetic helper
table:

- `1631` (**N_LXLSH@**): long signed `<<` uses the
  long-extended-left-shift helper, complement to
  `N_LXRSH@`. Same register ABI (DX:AX + CL → DX:AX).
- `1632` (**inline long compare** — no helper!):
  signed `a < b` for longs is inlined as a
  high-then-low two-step compare:
  ```
  mov ax, [a_high]
  mov dx, [a_low]
  cmp ax, [b_high]
  jg false       ; a_high > b_high → not less
  jl true        ; a_high < b_high → definitely less
  cmp dx, [b_low]
  jae false      ; equal high, but a_low >= b_low → not less
  true:
  mov ax, 1
  jmp done
  false:
  xor ax, ax
  done:
  ```
  High-word compare is **signed** (`jl`/`jg`); low-
  word fallthrough is **unsigned** (`jae`) since the
  low word has no independent sign bit. So BCC
  recognises that long compares are cheap enough to
  inline despite producing more bytes than a helper
  call would.
- `1633` (**N_LUDIV@**): unsigned long div uses a
  distinct helper from signed (`N_LDIV@`). Same
  stack-passed ABI, presumably self-clean.

Updated long-helper table:
| Helper      | Op           | ABI          |
|-------------|--------------|--------------|
| `N_LXMUL@`  | `long *`     | reg DX:AX,CX:BX |
| `N_LDIV@`   | signed /     | stack, self-clean |
| `N_LUDIV@`  | unsigned /   | stack, self-clean |
| `N_LXRSH@`  | signed >>    | reg DX:AX,CL |
| `N_LXLSH@`  | <<           | reg DX:AX,CL |
| `(none)`    | `long` cmp   | **inlined** high/low |

Still to probe: `N_LXURSH@` (unsigned >>), `N_LMOD@`
/ `N_LUMOD@` (mod variants).

## Long arithmetic helpers: `N_LXMUL@`, `N_LDIV@`, `N_LXRSH@`

Fixtures `1628` (`long * long`), `1629` (`long /
long`), and `1630` (`long >> int`) reveal the
library-helper-based ABI for 32-bit `long`
arithmetic. All pass on the first capture.

- `1628` (**N_LXMUL@**): `long * long` calls the
  `N_LXMUL@` helper. **Operands passed in registers**
  using a high:low word pair convention:
  - `cx:bx` = first operand (high:low)
  - `dx:ax` = second operand (high:low)
  - Result returned in `dx:ax`
  Long locals stored as 2 word slots: high word at
  lower offset, low word at higher offset (little-
  endian word ordering — low word at lower address).
  After call: `mov [bp-N], dx / mov [bp-N-2], ax` to
  store the 32-bit result.
- `1629` (**N_LDIV@**): `long / long` calls
  `N_LDIV@` with **stack-passed args** (four pushes
  for the two 32-bit operands, high-to-low order):
  ```
  push word [b_high] / push word [b_low]
  push word [a_high] / push word [a_low]
  call N_LDIV@
  mov [r_high], dx / mov [r_low], ax
  ```
  Result returned in `dx:ax`. **No caller arg cleanup
  visible** — the helper handles its own stack
  cleanup (presumably via `ret 8`).
- `1630` (**N_LXRSH@**): `long >> int` (signed)
  uses the long-extended-right-shift helper. ABI:
  - `dx:ax` = long value
  - `cl` = shift count (byte-load from int)
  - Returns `dx:ax`

So **long arithmetic uses two distinct calling
conventions**:
- Register-based (CX:BX, DX:AX → DX:AX) for **mul**
  and shifts.
- Stack-based (4 pushes → DX:AX) for **div** /
  **mod**.

The helper names follow a `N_L[X]<op>@` pattern.
Known helpers so far:
| Helper | Op | ABI |
|--------|-----|-----|
| `N_LXMUL@` | `long *` | CX:BX, DX:AX → DX:AX |
| `N_LDIV@`  | `long /` | stack-passed → DX:AX |
| `N_LXRSH@` | `long >>` (signed) | DX:AX + CL → DX:AX |

Likely more exist for: `N_LXLSH@` (shl), `N_LXURSH@`
(unsigned shr), `N_LMOD@` (mod), `N_LUDIV@` /
`N_LUMOD@` (unsigned div/mod), `N_LCMP@` (cmp), etc.
Worth probing.

## `c*c` int needs full promotion; signed vs unsigned char in mul

Fixtures `1625` (signed `char * char`, int result),
`1626` (unsigned `char * char`, int result), and
`1627` (`(char)(unsigned int >> 4)`) all pass on the
first capture.

- `1625`: each `char` operand is **promoted to int**
  before mul: `mov al,[bp-1] / cbw / push ax /
  mov al,[bp-2] / cbw / mov dx,ax / pop ax / imul
  dx`. So char*char with int result uses two `cbw`
  promotions plus the standard `imul`. The
  intermediate `push ax / pop ax` pair preserves
  the first promoted value across the second
  promotion (similar shape to bool-add in
  [[batch-412-shift-zero-boolsum-neg]]).
- `1626`: unsigned char promotion uses **`mov ah,
  0`** (zero-extend, 2 bytes) instead of `cbw` (1
  byte). For unsigned char `a` and `b`, the
  promotion is `mov al,[bp-1] / mov ah,0 / mov
  dl,[bp-2] / mov dh,0 / imul dx`. Notable: BCC
  inlines the second operand promotion into DL/DH
  directly (no push/pop), since the unsigned
  promotion clobbers no useful flags. Still uses
  `imul` (signed) — confirms [[batch-419-unsigned-
  mod-mul]] that mul codegen is signedness-
  agnostic.
- `1627`: `(char)(unsigned int >> 4)` lowers as `mov
  ax,[bp-2] / mov cl,4 / shr ax,cl / cbw`. The
  unsigned right shift uses `D3 /5` (`shr`, not the
  signed `sar`/`d3 f8`). Crucially, the narrowing
  cast does **not** propagate to byte-width even
  for unsigned `shr` — the cast pass excludes all
  shift-right ops regardless of signedness.

So the narrow-cast propagation rule from
[[batch-409-cast-shr-shl8]] is sharpened: SHR is
excluded **even when unsigned**. Only the signed/
unsigned choice of opcode (`sar` vs `shr`) is
affected by type signedness; the byte-width
optimisation remains opcode-keyed, not signedness-
keyed.

## Arg cleanup boundary: 3+ args → `add sp, N` (3 bytes)

Fixtures `1622` (3 args), `1623` (4 args), and
`1624` (5 args) probe the post-call arg-cleanup
boundary. All pass on the first capture:
- `1622` (3 args, 6 bytes): `add sp, 6` (`83 c4 06`,
  3 bytes — same as 3× `pop cx` but BCC chose the
  single instruction)
- `1623` (4 args, 8 bytes): `add sp, 8` (`83 c4 08`,
  3 bytes — saves 1 byte vs 4× pop)
- `1624` (5 args, 10 bytes): `add sp, 10` (`83 c4
  0a`, 3 bytes — saves 2 bytes vs 5× pop)

**Final arg-cleanup table**:
| Arg count | Bytes to clean | Encoding | Size |
|-----------|----------------|----------|------|
| 1 | 2 | `pop cx`           | 1 |
| 2 | 4 | `pop cx; pop cx`   | 2 |
| ≥ 3 | 2N | `add sp, 2N` (`83 c4 imm8`) | 3 |

So the cutover is at exactly 3 args: BCC prefers
pop chains for 1-2 args (1-2 bytes), and `add sp,
imm8` for 3+ args (3 bytes flat). The 3-arg case is
a tie in bytes (3× pop = `add sp, 6` = 3 bytes), and
BCC chose `add sp` — likely because it's a single
instruction with predictable timing on 8086. For
4+ args, `add sp` strictly wins.

The imm8 form `83 c4` (`add r/m16, imm8-sext`) is
the same encoding family as the imm8-sext arithmetic
ops ([[batch-400-imm8-policy]]). For args > 127
bytes (very rare in practice), it would need to
switch to imm16 form `81 c4 imm16` — not yet probed.

## `a[i]=99; a[i]` no CSE; 2-arg cleanup uses `pop cx; pop cx`

Fixtures `1619` (5-int array init), `1620` (`a[i]
= 99; return a[i]`), and `1621` (function via
out-param `compute(5, &x)`) all pass on the first
capture.

- `1619`: confirms `N_SCOPY@` for 5-int array,
  cx=10. The 5-element template `01 00 02 00 03 00
  04 00 05 00` is laid in `_DATA`.
- `1620` (**confirmation**): writing then reading
  the same `a[i]` with variable `i` emits the full
  address computation **twice** — no CSE.
  ```
  mov bx, si / shl bx, 1 / lea ax, [bp-6] / add bx, ax
  mov [bx], 99
  mov bx, si / shl bx, 1 / lea ax, [bp-6] / add bx, ax  ; ← recomputed!
  mov ax, [bx]
  ```
  Same "no CSE on indexed access" pattern seen in
  [[batch-384-2d-int-arr]] / fixture `1469`. The
  identical 8-byte address sequence is reemitted.
- `1621` (**finding**): for a 2-argument cdecl
  call, the post-call arg cleanup uses **`pop cx ;
  pop cx`** (2 bytes total) rather than `add sp,
  4` (3 bytes). So:
  | Arg cleanup size | Form | Bytes |
  |------------------|------|-------|
  | 2 bytes (1 arg) | `pop cx` | 1 |
  | 4 bytes (2 args) | `pop cx ; pop cx` | 2 |
  | 6 bytes (3 args) | (not yet probed; likely 3× `pop cx` or `add sp, 6`) |
  BCC prefers pop chains over `add sp, N` for small
  cleanup counts since pops are 1 byte each and
  `add sp, imm8` is 3 bytes.

Also notable from `1621`: function with a
**pointer-out-parameter** (`int *r`) enregisters
both params (`n` → SI, `r` → DI), confirms `*r =
...` lowering uses `mov [di], ax` (no extra mov
through AX since the writeback target is the
register itself). The body `*r = n*n + 1` lowers
as `mov ax, si / imul si / inc ax / mov [di], ax`
— clean four-instruction sequence.

## Arrays always use `N_SCOPY@`; structs use inline only for ≤2 fields

Fixtures `1616` (3-int struct), `1617` (2-int
array), and `1618` (1-int array) refine the
struct/array init lowering rule. **The threshold is
type-kind dependent**:

- `1618` (1-int array `int a[1] = {42};`) uses
  **`N_SCOPY@`** with cx=2.
- `1617` (2-int array `int a[2] = {10, 20};`) uses
  **`N_SCOPY@`** with cx=4.
- `1616` (3-int struct) uses **`N_SCOPY@`** with cx=6.

Combined with `1612` (1-int struct) and `1613` (2-int
struct) which **did not** use `N_SCOPY@`:

| Type | 1 word | 2 words | ≥3 words |
|------|--------|---------|----------|
| **Struct** | inline mov+store | 2× inline | `N_SCOPY@` |
| **Array**  | `N_SCOPY@`       | `N_SCOPY@` | `N_SCOPY@` |

So **arrays always go through `N_SCOPY@`** for brace
init, regardless of size — even `int a[1] = {42}`!
**Structs**, on the other hand, get inline load+store
pairs for 1- and 2-field cases.

This is a notable kind-dependent codegen split.
Likely an artifact of BCC's IR having distinct
initialiser paths for arrays vs structs: structs may
treat the brace init as a sequence of named field
assignments (which the small-size optimiser can
inline), while arrays use a uniform "copy from
template" path.

For the Rust reimplementation: pick the lowering
based on the **type kind** (struct vs array), not
just byte size.

## 2-field struct init avoids N_SCOPY; `(int)(5+3)` fully folded

Fixtures `1613` (`struct P {int x; int y;} p = {10,
20};`), `1614` (`int x = (int)(5 + 3);`), and `1615`
(`int x = (5 + 3);`) all pass on the first capture.

- `1613`: 2-int struct local init lowers to **two
  direct load+store pairs** — *not* `N_SCOPY@`. The
  template `0a 00 14 00` (10, 20) sits in `_DATA`,
  and the code emits:
  ```
  mov ax, [_template+2]   ; load second field (y=20)
  mov dx, [_template+0]   ; load first field (x=10)
  mov [bp-2], ax          ; store to p.y
  mov [bp-4], dx          ; store to p.x
  ```
  So fields are loaded **high-offset first** then
  low-offset, stored to their respective slots. The
  rule: 1-word struct uses 1 load+store, 2-word
  uses 2 load+stores. The `N_SCOPY@` helper kicks in
  somewhere between 2 and 3 words (3-int array uses
  N_SCOPY@, 2-int struct doesn't). So the threshold
  is **≥ 3 words → N_SCOPY@, ≤ 2 → inline pairs**.
- `1614` and `1615` are **byte-identical**:
  `(int)(5 + 3)` and `(5 + 3)` both fold to the
  constant 8 at parse time. The cast and the
  parentheses are both pure parser sugar with no
  codegen effect.

Updated struct/array init lowering threshold:
| Size (words) | Lowering |
|--------------|----------|
| 1 | direct `mov ax, [_template] / mov [bp-N], ax` |
| 2 | two load+store pairs (no helper) |
| ≥ 3 | `N_SCOPY@` memcpy helper |

This means for the encoder: pick the lowering style
based on the type's word count, not size in bytes.

## `(int)5` no-op cast, trailing comma in init, 1-field struct init

Fixtures `1610` (`int x = (int)5;`), `1611` (`int
a[3] = {1, 2, 3,};` trailing comma), and `1612`
(`struct S { int x; } s = {42};` single-field struct
init) all pass on the first capture.

- `1610`: `(int)5` cast is a complete codegen no-op
  — emits identical code to `int x = 5`. Same-type
  casts disappear at parsing.
- `1611`: trailing comma in brace initializer is
  accepted (a common C feature) and produces no
  extra array elements. Data is exactly `01 00 02
  00 03 00`. Code-equivalent to `{1,2,3}`.
- `1612` (**finding**): single-int-field struct local
  initializer **loads from a `_DATA` template via
  `a1 disp16 / mov [bp-2], ax`** (with FIXUPP) —
  NOT a direct `mov word [bp-2], 42` (which would be
  the same size but constant-immediate). So BCC
  uses the same "data-template + load+store" shape
  for *single*-word struct inits as it does for
  multi-word ones (via `N_SCOPY@`), just without
  the memcpy helper since the size is one word.
  This is mildly suboptimal vs. constant-imm-store
  but consistent — BCC treats struct init uniformly
  as data-template-copy. The template occupies 2
  bytes in `_DATA` for the int field.

So BCC's struct-init lowering rule:
- 1-word struct (or single field): `mov ax,
  [_template] / mov [bp-N], ax`
- N-word struct: `push ss / lea ax,[bp-N] / push ax
  / push ds / mov ax, _template / push ax / mov cx,
  N*2 / call N_SCOPY@`

The data-template approach is **always used** for
struct local init, even when a direct
constant-immediate store would be shorter or same
size.

## Switch on `char` (cbw + table); default-only (no dispatch); reorder

Fixtures `1607` (switch on `char` scrutinee with 4
dense cases), `1608` (switch with only a default
clause), and `1609` (4 dense cases in scrambled
source order: `3, 1, 2, 0`) all pass on the first
capture.

- `1607`: char scrutinee triggers **byte-load + `cbw`
  promotion** before the standard jump-table
  dispatch: `mov al, [bp-1] / cbw / mov bx, ax / cmp
  bx, 3 / ja default / shl bx, 1 / jmp cs:[bx +
  table]`. The promotion is essentially zero-cost.
  Negative chars (with sign-extended high byte set)
  are correctly treated as out-of-range by the
  unsigned `ja` bounds check.
- `1608`: a switch containing **only `default:`** has
  **no dispatch at all** — the scrutinee is
  evaluated (stored to its slot if it has side
  effects) but never tested. The default body runs
  unconditionally. Two `eb 00` no-op jumps remain
  as artifacts of the loop/dispatch template
  (one between scrutinee setup and body, one after
  body before end label) — consistent with BCC's
  "always emit template skeleton" style.
- `1609` (**important**): cases declared out of
  source order (e.g. `case 3, case 1, case 2, case
  0`) produce **case bodies in source order** but
  **jump-table entries sorted by case value**. The
  table indexed by value `i` always points at the
  body for `case i`, regardless of which position
  it appears in the source. So:
  - Body layout: source order
  - Table layout: sorted by case value
  This means the encoder must sort cases by value
  when generating the table, while emitting bodies
  in source order with forward `jmp` to the
  end_switch label.

Updated final switch dispatch rules:
- ≤ 3 cases: linear chain (tested in source order)
- ≥ 4 dense: indexed jump-table (sorted by value,
  bodies in source order)
- ≥ 4 sparse: linear-search CS-table (sorted by
  value)
- char scrutinee: prefix `cbw` to promote to int
  before any of the above
- only `default`: no dispatch, body runs
  unconditionally

## Switch dispatch: 3 strategies — linear, indexed-table, search-table

Fixtures `1604` (4 sparse cases), `1605` (4 dense
cases with non-zero base), and `1606` (3 dense
cases with default) all pass on the first capture
and complete the switch-dispatch classification:

**Three distinct dispatch strategies:**

1. **Linear cmp-jcc chain** (≤ 3 cases): each case
   tested in turn with `cmp ax, value / je
   case_body`. Default falls through. (Fixtures
   `1598`, `1599`, `1600`, `1606`.)

2. **Indexed jump-table** (≥ 4 *dense* cases):
   - For 0-based dense (`case 0; case 1; ...`):
     `cmp bx, max / ja default / shl bx, 1 / jmp
     cs:[bx + table]`.
   - For non-zero-base dense (`case 5; case 6;
     ...`): identical but with a prefixing `sub
     bx, base` to normalise the index to 0..N-1.
   - Table holds N word-sized target offsets.
3. **Linear-search CS-table** (≥ 4 *sparse* cases —
   `1604`): novel third strategy! BCC emits a
   linear-search loop using the 8086 `LOOP`
   instruction:
   ```
   mov [bp-4], scrutinee   ; save to stack
   mov cx, N               ; number of cases
   mov bx, table_offset    ; CS-table base
   search:
     mov ax, cs:[bx]       ; read case value
     cmp ax, [bp-4]        ; compare to scrutinee
     je found
     inc bx; inc bx        ; advance by 2
     loop search           ; LOOP: dec cx, jnz
   jmp default
   found:
     jmp cs:[bx + 2*N]     ; jump through paired
                           ; target offset
   ```
   The CS-table stores the N case values
   followed by N target offsets. The `jmp cs:
   [bx + 2*N]` indexes 2*N bytes past the
   matched value to find its corresponding target.

So the lowering decision is:
- ≤ 3 cases: linear chain
- ≥ 4 dense consecutive: indexed jump-table (with
  optional base subtract)
- ≥ 4 not-dense: linear-search CS-table with `LOOP`

The sparse strategy trades O(N) lookup time for
compact table size (no gaps).

## Switch dispatch cutoff: **4 dense cases triggers jump-table**

Fixtures `1601` (4 dense), `1602` (5 dense), and
`1603` (6 dense) all use a **jump-table dispatch**,
while `1598` (2 cases) and `1600` (3-4 with sparse
intent) and `132/072` (3 dense cases) all use linear
cmp-jcc. The cutoff: **4 or more consecutive dense
cases** triggers BCC's jump-table dispatch.

Jump-table dispatch shape (from `1601`):
```
8b 5e fe       mov bx, [bp-2]      ; scrutinee → BX
83 fb 03       cmp bx, 3           ; compare to max case
77 1b          ja default          ; unsigned above → default/end
d1 e3          shl bx, 1           ; scale by 2 (word offsets)
2e ff a7 d16   jmp cs:[bx + table] ; indirect jmp through CS-prefixed
                                   ; table base address
```
Followed by case bodies, with a 2-byte-per-entry
table in the code segment containing the offset of
each case's body label (relative to CS).

Notable details:
- **Bounds check is unsigned** (`ja`, opcode `0x77`)
  — `case 0` is always covered, so anything `<0`
  (signed-wise) is wraps to "above 3" in unsigned
  terms and goes to default. So negative scrutinees
  also go to default.
- **`shl bx, 1`** scales the index since table
  entries are 2 bytes (word offsets).
- **`2e` prefix** (CS segment override) — the jump
  table lives in the code segment (`_TEXT`), so the
  indirect `jmp [bx + disp]` reads from CS.
- The table itself has **2 bytes per case** + 0
  bytes for default (default just falls through the
  initial bounds check).

So the lowering rule:
- Dense 0..N-1 cases with N ≤ 3: linear cmp-jcc chain
- Dense 0..N-1 cases with N ≥ 4: jump-table via
  `cmp / ja / shl / jmp cs:[bx + table]`

Sparse case sets (e.g. `case 1; case 5; case 10`)
likely use linear chain regardless of count — needs
a sparse-case probe to confirm.

## Small switch: linear cmp-jcc chain; `case 0` uses `or ax,ax` shortcut

Fixtures `1598` (2 cases no default), `1599` (1
case + default), and `1600` (4-case switch on array
element) all pass on the first capture.

- `1598`: small switch (2-3 cases) lowers as a
  **linear cmp-jcc chain**, not a jump table. The
  scrutinee is loaded into AX, then each case is
  tested in order with `cmp / je case_body`. After
  all tests fail, an unconditional `jmp end` (with
  no `default` clause) or `jmp default_body` takes
  the fallthrough.
- **`case 0` special-cased**: `or ax, ax / je`
  (2-byte test) is used for the zero case instead of
  the longer `cmp ax, 0 / je`. So switch-on-zero
  gets the same 2-byte truthiness check as
  `if (x)`.
- Other case values use **`cmp ax, imm16`** via the
  AX-specific `0x3D` short opcode (3 bytes total,
  even for imm fitting in imm8 sign-ext range). BCC
  canonicalises on `0x3D` for AX-with-imm cmp,
  matching the AX-with-imm `add`/`sub`/`or` family
  policy from [[batch-400-imm8-policy]].
- Each case body ends with `jmp end_switch`
  (joining all paths to a single end label). The
  case bodies are laid out in source order *after*
  the dispatch chain, with the end label after
  them.
- `1600`: confirms the same pattern for 4 cases +
  default. Each cmp-jcc against {1, 2, 3} in order;
  fallthrough goes to default. Body labels are
  forward jumps from the dispatch.

For larger switches (e.g. `072-switch-many-dense`
not re-probed here), BCC uses a different jump-
table strategy. The cutover between linear-chain
and jump-table likely correlates with case
density/count — needs a dedicated probe to
characterise.

## do-while keeps body-first shape; side-effect in cond saves old value

Fixtures `1595` (`do { i++; } while (i < 3);`),
`1596` (`while (i < 3) { s += i; i++; }` multi-stmt
body), and `1597` (`while (i++ < 3);` side-effect in
cond) all pass on the first capture.

- `1595` (**finding**): `do { ... } while (cond)`
  is the **one loop form that keeps its own shape**.
  Lowering is `init / body / cmp / jcc back` —
  **no leading `jmp test`** like the
  while/for variants. The body runs once
  unconditionally, then the test follows. This
  matches the natural `do-while` semantics
  (post-test loop) and is distinct from the bottom-
  test pattern of the other forms.
- `1596`: multi-statement while body — standard
  bottom-test shape, both i and s enregister into
  SI and DI (both multi-use). Body is just two
  instructions (`add di, si / inc si`), then test.
- `1597` (**finding**): `while (i++ < 3)` with the
  side effect inside the condition lowers to:
  ```
  mov ax, si      ; save current i for compare
  inc si          ; i++ side effect
  cmp ax, 3       ; compare OLD i against 3
  jl back         ; loop if old i < 3
  ```
  The postfix-increment saves the pre-increment
  value into AX *before* applying the increment to
  SI, then compares the saved AX. This correctly
  implements the postfix `i++` semantics (uses old
  value, then increments). A *leading* `eb 00`
  (jmp to next instruction, 2 useless bytes) is
  emitted because the canonicalisation always
  inserts the "jmp test" at the top, even when the
  body and test are the same instructions — a
  systematic source of dead jumps.

Final loop-form lowering catalog (six base shapes):
| Form | Canonical lowering |
|------|--------------------|
| `if (cond) X else Y` | `cmp / inv-jcc L_else / X / jmp end / L_else: Y` |
| `while (cond) X`     | `jmp test / X: ... / test: cmp / jcc back` |
| `for (init; cond; incr) X` | `init / jmp test / X: incr / test: cmp / jcc back` |
| `do { X } while (cond)` | `X / cmp / jcc back` (no leading jmp!) |
| `while (1)` / `for (;;)` | `body / jmp back` (no test) |
| `do { X } while (0)` | `X` only (no overhead) |

## Bounded loops: `while`/`for` all canonicalise to bottom-test pattern

Fixtures `1592` (`while (i < 3) i++;`), `1593` (`for
(i = 0; i < 3; i++);` empty body), and `1594` (`while
(i < 3) { i++; }`) all emit **byte-identical code**
to each other. Combined with the existing for-loop
fixtures (`1205`, `1500`, etc.), the bottom-test
canonical pattern is now confirmed across all bounded
loop forms:

```
xor si, si      ; init
eb 01           ; jmp test
46              ; body: inc si  (or other body)
83 fe 03        ; test: cmp si, 3
7c fa           ; jl body  (back-edge with signed-less)
```

So BCC's loop normaliser unifies all of these into
the same shape:
| Source form | Internal IR |
|-------------|-------------|
| `while (cond) body` | `for ( ; cond ; ) body` |
| `for (init; cond; incr) body` | as-is |
| `while (cond) { body; incr; }` | same as for-loop |

The "incr" expression goes at body-tail (just before
test) regardless of whether it came from a for-incr
clause or was written explicitly at end of body.
The test goes at the bottom; entry is via `jmp test`
to skip the body before first iteration.

For the Rust reimplementation, this means the IR
must:
1. Rewrite `while (cond) body` as a for-loop with no
   init/incr but with same body.
2. Always emit bottom-test pattern with forward jmp
   on entry.

The earlier-batch finding that infinite-loop variants
([[batch-424-infinite-loops]]) all canonicalise to a
*top-test* pattern (since the cond is trivially true,
no test needs to be done; just jmp back from body
tail) is the degenerate case of this same
normalisation rule.

## Infinite-loop forms all canonicalise to identical bytes

Fixtures `1589` (`do { ... } while (1);`), `1590`
(`for (i=0; ; i++) { ... }`), and `1591`
(`for (;;) { ... }`) all pass on the first capture
and **emit byte-identical code** to each other and to
`1586` (`while (1) { ... }`). All four lower to:

```
prologue + push si
33 f6        xor si, si            ; i = 0
83 fe 03     cmp si, 3             ; loop_top:
75 02        jne body
eb 03        jmp loop_end          ; break
              body:
46           inc si                ; i++
eb f6        jmp loop_top
              loop_end:
8b c6        mov ax, si
eb 00        ret
```

So BCC's IR **canonicalises all "infinite loop" source
forms** (`while(1)`, `do...while(1)`, `for(;;)`,
`for(init; ; incr)`) into the same internal loop
shape: a test-position-at-top loop with the
`break`-cmp inside the body. Even the syntactic
difference between an explicit `for`-increment
clause and a body-tail post-increment collapses
into the same encoding.

This implies the IR has a **loop normaliser** that:
1. Recognises constant-true conditions and removes
   them.
2. Promotes the `for`-incr expression into the body
   tail (so the body becomes `body; incr;`).
3. Emits a single template: `init / test-loop_top:
   body / jmp loop_top / loop_end:`.

For the Rust reimplementation, the loop-IR layer
must perform this normalisation **before** codegen
to match BCC byte-exact for all infinite-loop
fixtures.

## const-cond loops: `while(1)` → `jmp` back; `while(0)` skips; `do…while(0)` no test

Fixtures `1586` (`while (1) { ... break; }`), `1587`
(`while (0) i++;`), and `1588` (`do i++; while (0);`)
all pass on the first capture, covering the
constant-condition loop forms.

- `1586`: `while (1)` lowers to an unconditional
  **`jmp` back to the loop top** — no cmp/jcc for
  the test. `break` is `jmp loop_end` jumping past
  the back-edge. Cleanest of the three patterns.
- `1587`: `while (0)` lowers to a forward **`jmp $+1`
  over the body** (dead code). The body `inc si`
  is still emitted but unreachable. Same shape as
  `if (0)` from `1585`.
- `1588`: **`do { ... } while (0)`** lowers to
  *just the body* — **no test or jump emitted**.
  This is the idiomatic "execute body exactly
  once" form used in macros, and BCC recognises it
  fully (test folded AND no back-edge generated).

So the constant-cond lowering table:
| Form | Lowering |
|------|----------|
| `if (1)` | true body, jmp over dead false body |
| `if (0)` | jmp over dead true body, false body |
| `while (1)` | body + jmp back (no test) |
| `while (0)` | jmp over dead body |
| `do…while (0)` | body only (zero overhead) |
| `do…while (1)` | (not yet probed; likely body + jmp back) |
| `for (;;)` | (not yet probed; likely body + jmp back) |

The do-while(0) case is the only one without dead
code emission — because there's no body to skip
(the body is what runs), and no back-edge to
generate (cond is false so no loop).

## const-arith folded; `if (1)`/`if (0)` test folded but dead code emitted

Fixtures `1583` (`int x = 100 - 7 * 3`), `1584` (`if
(1) return 5; return 10;`), and `1585` (`if (0)
return 5; return 10;`) all pass on the first capture
and characterise BCC's constant-folding scope.

- `1583`: full compile-time arithmetic folding —
  `100 - 7 * 3` reduces to **79 (0x4F)** stored
  directly into x's slot. The AST/parser layer
  evaluates constant expressions before reaching
  codegen.
- `1584`: `if (1)` lowers to **`mov ax, 5 / jmp $+5
  / mov ax, 10 / jmp epilogue`**. The test is
  folded away (no `cmp` / `jcc`), but the dead
  branch (`mov ax, 10`) is still emitted as
  unreachable code. The `jmp` skips 5 bytes — the
  exact length of the dead branch.
- `1585`: `if (0)` lowers to **`jmp $+5 / mov ax, 5
  / jmp epilogue / mov ax, 10`**. The test fold
  emits an unconditional `jmp` to skip the dead
  true branch, then falls through to the false
  branch.

So constant folding in BCC is **partial**: numeric
expressions are fully evaluated (as in `1583`); but
for `if (const)` the dead branch is still encoded as
unreachable code — only the *test* is skipped. The
encoder's IR doesn't have a "DCE after constant
fold" pass. The Rust reimplementation must match
this: emit both branches and connect them with the
appropriate `jmp` instead of cmp/jcc.

Combined with the [[batch-421-two-calltargets-strcond]]
finding that `if ("X")` is *not* folded at all
(emits the full template), the constant-folding
boundary is:
- Numeric/arithmetic operands: fully folded
- `if (numeric_const)`: test folded, dead branch
  kept
- `if (literal_string)`: not folded; full template

## 2 call-targets: decl order; call+binop chains; `if ("X")` not folded

Fixtures `1580` (2 multi-use locals both used as
call-targets), `1581` (`int x = seven() + 3` — init
from call-then-binop), and `1582` (`if ("X")` —
string-literal as if condition) all pass on the
first capture.

- `1580` (**resolves open question**): when 2 locals
  are both reassigned by call-returns (both
  "call-targets"), they get SI and DI in
  **declaration order** — `a` → SI, `b` → DI. The
  earlier hypothesis from [[batch-397-call-cross]]
  that "the call-target gets SI" only applied when
  *exactly one* of the multi-use locals is a
  call-target (in `1508`/`1510` only `c`/`d`
  respectively was a call-target; the non-call-
  target locals got DI). With multiple call-
  targets competing, plain declaration order wins.
- `1581`: a call result chains directly into a
  follow-on binop: `call _seven / add ax, 3 / mov
  [bp-2], ax`. No intermediate save — AX is the
  call's return register and stays live for the
  immediate `add`. So `f() + K` (or `f() op K` in
  general) lowers cleanly to call-then-op.
- `1582` (**missed optimisation**): `if ("X")` does
  **not** get folded to constant-true. BCC emits
  the full template: `mov ax, offset"X" / or ax,ax
  / je L_else / mov ax,1 / jmp / xor ax,ax`. The
  string-literal pointer is a known-non-null
  compile-time value (C guarantees it), but BCC
  doesn't recognise this in the IR — it emits the
  generic truthiness test. (Note: at runtime the
  test will succeed since linker resolves the
  pointer to a non-zero address, but the test is
  still wasted code.)

## unsigned `/7` `xor dx,dx / div bx`, unsigned `<` uses `jae` inverse

Fixtures `1577` (`unsigned v / 7`), `1578` (`unsigned
a < unsigned b`), and `1579` (`unsigned v % 7`) all
pass on the first capture and complete the signed-vs-
unsigned arithmetic codegen calibration.

- `1577`: unsigned non-pow2 div is **`xor dx, dx /
  div bx`** — `xor dx,dx` zeroes the high word for
  unsigned dividend, then `div r/m16` (opcode `F7
  /6`, unsigned). No `cwd` (which would
  sign-extend AX to DX:AX, wrong for unsigned).
- `1578` (**finding**): unsigned `<` uses **`jae`**
  (opcode `0x73`, jump-above-or-equal, the inverse
  of `jb`) for the bool materialization template.
  Compare to signed `<` which uses `jge` (opcode
  `0x7D`). So jcc selection tracks signedness end-
  to-end. Full inverse-jcc table for `if (a OP b)
  return 1`:
  | C op | signed | unsigned |
  |------|--------|----------|
  | `<`  | `jge` (7D) | `jae` (73) |
  | `<=` | `jg`  (7F) | `ja`  (77) |
  | `>`  | `jle` (7E) | `jbe` (76) |
  | `>=` | `jl`  (7C) | `jb`  (72) |
  | `==` | `jne` (75) | `jne` (75) |
  | `!=` | `je`  (74) | `je`  (74) |
- `1579`: unsigned non-pow2 mod uses the same
  div pattern as `1577`, then **`mov ax, dx`** to
  move the remainder from DX (where 8086 `div`
  leaves it) into AX (return register). So mod
  differs from div only in the trailing `mov ax,
  dx` (3 bytes added).

Updated arithmetic-codegen table is now complete:
| Op | Signed | Unsigned |
|----|--------|----------|
| `*K` (pow2) | `shl ax, log2 K` | same |
| `*K` (other) | `mov dx,K / imul dx` | same |
| `/K` (pow2)  | `cwd / idiv bx`     | `shr ax, log2 K` |
| `/K` (other) | `cwd / idiv bx`     | `xor dx,dx / div bx` |
| `%K` (pow2)  | `cwd / idiv bx`     | `and ax, K-1` |
| `%K` (other) | `cwd / idiv bx / mov ax,dx` | `xor dx,dx / div bx / mov ax,dx` |
| `<` jcc      | `jge` inv           | `jae` inv |
| ... etc     | ...                | ... |

## unsigned `%2` → `and ax,1`; mul always `imul` regardless of signedness

Fixtures `1574` (`unsigned int v; return v % 2;`),
`1575` (`unsigned int v; return v * 2;`), and `1576`
(`unsigned int v; return v * 3;`) all pass on the
first capture.

- `1574` (**finding**): unsigned mod-by-pow2 K
  lowers to **`and ax, K-1`** — for K=2 this is
  `25 01 00` (`and AX, imm16`, the 3-byte short
  form, opcode `0x25`). Saves 6+ bytes over the
  `cwd / idiv bx` shape used for signed mod. So:
  | Type / op | Codegen |
  |-----------|---------|
  | signed `%2`   | `mov bx,2 / cwd / idiv bx` |
  | unsigned `%2` | `and ax, 1`                |
- `1575`: unsigned `v * 2` lowers to **same**
  `shl ax, 1` as signed (`D1 /4`). Mul-by-pow2
  ignores signedness — addition/shifting is closed
  mod 2^k for both.
- `1576` (**finding**): unsigned `v * 3` uses
  **`imul`** (signed mul, `F7 /5`), not `mul`
  (unsigned mul, `F7 /4`). The bytes are `mov dx,
  3 / imul dx` — same as signed `1520`. BCC always
  uses signed `imul` for multiplication regardless
  of operand signedness, because the low 16 bits
  of the product are identical whether `imul` or
  `mul` is used (C requires only the low word for
  int*int truncation). So **mul codegen does not
  distinguish signed/unsigned** — only div/mod does.

Updated summary table for signedness-dependent
arithmetic:
| Op | Signed lowering | Unsigned lowering |
|----|-----------------|-------------------|
| `*K` | `imul` (or `shl` for K=pow2) | `imul` (or `shl` for K=pow2) — same |
| `/K` (K=pow2) | `cwd / idiv bx` | `shr ax, log2(K)` |
| `/K` (K≠pow2) | `cwd / idiv bx` | `xor dx,dx / div bx` |
| `%K` (K=pow2) | `cwd / idiv bx` (remainder in DX) | `and ax, K-1` |
| `%K` (K≠pow2) | `cwd / idiv bx` (remainder in DX) | `xor dx,dx / div bx` (DX) |

(Last column for non-pow2 unsigned div/mod not yet
probed but consistent with the 8086 ABI.)

## `v*2` → `shl ax,1`; signed `/2` → `idiv`; unsigned `/2` → `shr ax,1`

Fixtures `1571` (`v * 2` signed int), `1572` (`v / 2`
signed int), and `1573` (`v / 2` unsigned int) all
pass on the first capture and complete the
mul/div-by-pow2 picture.

- `1571`: signed `v * 2` lowers to `shl ax, 1`
  (opcode `D1 /4`, 2 bytes). BCC preferred `shl ax,
  1` over the equivalent `add ax, ax` (`03 C0`,
  also 2 bytes) — encoder canonicalises on shl
  for mul-by-pow2 regardless of size tie.
- `1572` (**signed div not shortcut**): `v / 2`
  with signed int lowers to **`mov bx, 2 / cwd /
  idiv bx`** — full word-width signed division
  using `idiv r/m16` (`F7 /7`). BCC does NOT
  shortcut signed div-by-pow2 to `sar`, because
  `sar` rounds toward `-∞` for negatives while C
  signed div rounds toward zero. So divs of
  potentially-negative values must use real div.
- `1573` (**unsigned div IS shortcut**): `v / 2`
  with unsigned int lowers to just `shr ax, 1`
  (opcode `D1 /5`, 2 bytes). Unsigned div-by-pow2
  is safe to lower to `shr` because both treat the
  word as zero-extended positive, and `shr` rounds
  toward zero (always positive). No `cwd` or
  `idiv` instructions needed.

This is one of the largest signed-vs-unsigned codegen
differences. For the Rust reimplementation:
- mul-by-pow2: always `shl REG, log2(K)` (signed or
  unsigned doesn't matter for mul under truncation).
- div-by-pow2: signed → `cwd / idiv`; unsigned →
  `shr REG, log2(K)`.
- mod-by-pow2: signed → `cwd / idiv` (preserved as
  remainder in DX). Unsigned → could use `and REG,
  K-1` but BCC's behaviour for unsigned mod-by-pow2
  not yet probed.

## inc/dec shortcut: confirmed split — compound `±= 1` direct, longhand `= ± 1` round-trip

Fixtures `1568` (`i = i + 1`), `1569` (`i -= 1`),
and `1570` (`i--`) directly compare the three forms
of "decrement by 1" against a register-allocated
local. All pass on the first capture.

- `1568` (`i = i + 1`): `mov ax,si / inc ax / mov
  si,ax` — **6 bytes**, AX round-trip. Confirms
  the finding from [[batch-416-arr-of-ptrs-early-
  return-loop-break]] / fixture `1567`.
- `1569` (`i -= 1`): just `dec si` — **1 byte**,
  direct on home register.
- `1570` (`i--`): just `dec si` — **1 byte**,
  direct.

So 1569 and 1570 produce **byte-identical codegen**
for the body, but 1568 is 5 bytes longer despite
semantically identical behaviour. The split is by
*syntactic form*:
| Form | Codegen |
|------|---------|
| `i++`, `++i`, `i += 1` | `inc REG` (1 byte) |
| `i = i + 1`            | `mov ax,REG / inc ax / mov REG,ax` (6 bytes) |
| `i--`, `--i`, `i -= 1` | `dec REG` (1 byte) |
| `i = i - 1`            | `mov ax,REG / dec ax / mov REG,ax` (6 bytes) |

For byte-exact Rust reimplementation, the parser/IR
must distinguish these forms — the
"semantically equivalent" rewrites a modern compiler
would unify must NOT be performed. The dec/inc-on-
home-register pattern is opcode `0x40 + reg` (inc)
and `0x48 + reg` (dec).

## `int *p[2]`, early `return` from void, `i = i - 1` misses `dec si`

Fixtures `1565` (array of int pointers — `int
*p[2]; p[0]=&a; p[1]=&b; *p[1]=99`), `1566` (early
`return;` from void function), and `1567` (do-while
with `break` inside an if) all pass on the first
capture.

- `1565`: confirms `int *p[2]` lowering — two
  pointer slots laid out contiguously in the stack
  frame. `p[0] = &a` is `lea ax,[bp-2] / mov
  [bp-8], ax` (the second-from-top slot). `*p[1] =
  99` lowers to **`mov bx,[bp-6] / mov [bx],
  99`** — uses BX as the deref base register. Even
  with no enregistration of `p` itself (it's a
  stack array), each indexed pointer is loaded
  through BX for the write.
- `1566`: an early `return;` from a void function
  lowers to **`jmp epilogue`**. The body code is
  `if (cond) jmp epilogue / else { ... } / epilogue:`.
  No extra "tail return" handling, no marker —
  just an unconditional jump to the function's
  prologue-matching epilogue label. Matches
  conventional C codegen.
- `1567` (**finding**): in a do-while loop body,
  `i = i - 1` lowers to **`mov ax, si / dec ax /
  mov si, ax`** (3 instructions, 6 bytes) — NOT
  `dec si` (1 instruction, 1 byte). BCC's `dec`/
  `inc` shortcut applies only to `++`/`--` and
  compound `+=`/`-=` operators, not to longhand
  `i = i - 1`. The IR parses the latter as a
  generic `assign(i, sub(i, 1))` and lowers it via
  the AX-round-trip RMW shape. So:
  - `i--`, `--i`, `i -= 1` → `dec si` (1 byte)
  - `i = i - 1` → `mov ax,si / dec ax / mov si,ax`
    (6 bytes)
  The semantics are identical but the codegen
  differs by 5 bytes. `break` from a do-while loop
  lowers to `jmp end_loop_label`, jumping past the
  while-test directly to the epilogue.

## ptr cast no-op, `c + 3` cbw-then-add, `a[1]` returns `[bp-4]` direct

Fixtures `1562` (`char *p = (char *)&x; return *p;`),
`1563` (`char c=5; int i = c + 3; return i;`), and
`1564` (`int a[3]; ...; return a[1];`) all pass on
the first capture.

- `1562`: pointer-type cast `(char *)&x` is a
  **codegen no-op** — just affects how subsequent
  derefs interpret width. `&x` is `lea ax,[bp-2] /
  mov si,ax` (the usual address setup), then `*p`
  is `mov al,[si] / cbw` (byte load because `p` is
  now `char *`). Returns 0x34, the low byte of x.
- `1563`: `c + 3` where c is char and result is int
  triggers standard C integer promotion: `mov al,
  [bp-1] / cbw / add ax, 3 / mov [bp-4], ax`. The
  `cbw` promotes char to int *before* the add, and
  the add then operates at word width using
  `0x05 imm16`. This is the **inverse** of the
  byte-propagation pass ([[batch-407-cast-binop-
  table]]): when no narrowing cast surrounds the
  expression, BCC always promotes char to int and
  computes at word width.
- `1564`: stack array `a[N]` with constant index N
  uses **fully folded** `[bp+disp]` for the
  element. `a[1]` is `[bp-4]` (base [bp-6], +1 * 2
  bytes), with no `lea`/`add`/`shl` scaling at run
  time. Just a direct memory access.

These three fill in details that complete the basic
type-conversion picture for the encoder.

## `v >>= 1` direct `sar si,1`, `if (reg-x)` uses `or si,si` shortcut

Fixtures `1559` (`v >>= 1` with v in SI), `1560`
(`register int x; if (x)` with x in SI), and `1561`
(`v &= 1` with v in SI) all pass on the first
capture.

- `1559`: confirms direct-on-home shift for **SAR**
  too. `v >>= 1` lowers to `sar si, 1` (`D1 /7`,
  ModR/M `FE`). Same shape as `shl si, 1` (`D1
  /4`) from `1557`. So shift compound ops in both
  directions skip the AX round-trip.
- `1560` (**finding**): for a register-allocated
  local, `if (x)` uses **`or si, si`** (`0B F6`, 2
  bytes) instead of `cmp si, 0` (3 bytes with
  `83 FE 00` imm8-sext or 4 bytes with imm16).
  Saves 1 byte and produces the same flags. So
  truthiness-against-zero uses different opcodes
  based on operand location:
  - Memory operand: `cmp [m], 0` (`83 /7 disp 00`)
  - Register operand: `or REG, REG` (`0B mod=11
    rm=reg/2-bytes`)
- `1561`: confirms the imm16 AND encoding from
  [[batch-400-imm8-policy]]. `v &= 1` with v in SI
  emits `81 e6 01 00` (4 bytes), **not** the
  legal-but-shorter `83 e6 01` (3 bytes imm8-sext).
  The bitwise ops still always use `81 /N` imm16
  form regardless of immediate value.

Combined with the earlier batch findings, BCC's
zero-test pattern is fully calibrated:
| Operand location | Encoding | Bytes |
|------------------|----------|-------|
| Register (SI/DI/DX/BX/CX) | `or REG, REG` (`0B`) | 2 |
| Memory `[bp+disp]` | `cmp [bp+disp], 0` (`83 /7`) | 4 (disp8) |
| Memory direct `[m]` | `cmp [m], 0` (`83 /7`) | 5 (disp16) |

## `v = ~v` via `not ax`, `v <<= 1` direct `shl si,1`, `if (x)` via `cmp [m],0`

Fixtures `1556` (`v = ~v`), `1557` (`v <<= 1` with v
in SI), and `1558` (`if (x) return 1` with x on
stack) all pass on the first capture.

- `1556`: `~v` uses **`not ax`** (`F7 /2`, opcode
  byte `D0`) via the AX round-trip — same shape as
  `neg ax` (`F7 /3`) from [[batch-412-shift-zero-
  boolsum-neg]] fixture `1555`. So single-operand
  unary ops (`neg`, `not`) consistently use AX
  round-trip when operating on a register-allocated
  local.
- `1557` (**inconsistency**): `v <<= 1` lowers to
  **`shl si, 1`** (`D1 /4`, ModR/M `E6`) — direct on
  the home register SI, **no** AX round-trip. So
  the AX-round-trip pattern does *not* apply to
  shift compound ops — shifts target the home
  register directly. Likely because BCC's shift
  emission is special-cased (the shift count is
  fixed in CL, so the destination register is
  always free to be the home).
- `1558`: `if (x)` with x at `[bp-2]` lowers to
  `cmp [bp-2], 0 / je L_else / mov ax,1 / jmp / xor
  ax,ax`. The cmp uses the **`83 /7` imm8-sext
  form** (4 bytes including disp8) against
  immediate 0 — no shortcut to `or ax, ax` (which
  would require loading first anyway). So
  truthiness against a memory operand is the
  natural `cmp r/m, 0`, not load-then-test.

Updated AX-round-trip vs direct-register table:
| Op             | AX round-trip? | Direct on home? |
|----------------|----------------|------------------|
| Unary `~`      | yes            | no               |
| Unary `-`      | yes            | no               |
| Shift `<<= 1`  | no             | yes (`D1 /4`)    |
| Shift `>>= 1`  | (likely yes/no?)| not probed      |
| `++` / `--`    | no             | yes (`inc si`)   |
| Binop with mem | yes            | no               |
| `lea` setup    | yes            | no               |

So the AX-round-trip is selective — short ops with
1-byte forms (`inc`, `dec`) and shift-with-immediate
get direct-on-home emission; longer single-op
patterns (`neg`, `not`, mem binops) go through AX.

## `v<<0` folded away, two bool-cmp adds via push/pop, `v=-v` via `neg ax`

Fixtures `1553` (`return v << 0;` — shift by zero),
`1554` (`(a == b) + (c == d)` — sum of two
materialized bools), and `1555` (`v = -v;` — negate
in place) all pass on the first capture.

- `1553`: `v << 0` is fully **folded to identity** —
  no `shl` or `mov cl, 0` emitted. Just `mov ax,
  [bp-2]` for the load. BCC's IR has a "shift by 0
  → operand pass-through" rewrite.
- `1554`: each `(a == b)` and `(c == d)` materializes
  via the standard `cmp / jne / mov ax,1 / jmp / xor
  ax,ax` template. To combine them, BCC emits the
  first bool, **pushes AX to preserve it across the
  second template**, emits the second bool, copies
  it to DX, **pops the first bool back into AX**,
  then `add ax, dx`. So the inter-template
  preservation uses the stack — not a stack-local
  slot, but raw `push ax / pop ax` — even though
  a free register (e.g. SI/DI/BX) could have held
  it. This suggests no IR-level value tracking
  across the second cmp materialization: BCC treats
  each bool template as opaque/clobbering of all
  scratch regs.
- `1555`: `v = -v` lowers to `mov ax, si / neg ax /
  mov si, ax` — the standard RMW-via-AX pattern with
  `neg r/m16` opcode `0xF7 /3`. BCC could have
  emitted `neg si` directly (1 byte less), but
  routes through AX for consistency with the
  general RMW shape — same inefficiency seen with
  `lea ax, [bp-N] / mov si, ax` for pointer setup
  ([[batch-384-2d-int-arr]]).

So BCC's codegen has a few systematic
"AX-round-trip" inefficiencies: pointer-setup,
unary-op-on-register, simple binop-on-register, etc.
The pattern is "compute into AX, mov to home reg" —
even when the home reg could be the direct
destination of the operation. Worth replicating for
byte-exactness.

## `register` overrides use-count; `*a++` int-ptr `inc si/inc si`

Fixtures `1550` (`register int x = 5; return x;` —
register keyword with single use), `1551` (`sum_n`
with `while (n--) s += *a++;`), and `1552` (two
globals `a = 3; b = 4; return a + b;`) all pass on
the first capture.

- `1550` (**finding**): the `register` keyword
  **forces enregistration** even when the
  use-count rule would not promote. `x` has only 1
  syntactic use (the return) — normally it would
  stay on stack — but with `register int x` it goes
  to SI. So `register` is an explicit override
  ("yes please enregister this") that complements
  the implicit forcing flags (volatile prevents
  enregistration, address-taken prevents it). The
  hint *is* honored by BCC 2.0, unlike some later
  compilers that ignore it.
- `1551` shows the canonical "pointer + count loop":
  `a` → SI (read+inc), `n` → DX (dec+test, scratch
  reg used because no calls), `s` → DI (compound +=).
  Two notable lowerings:
  - **`a++` for `int *` advances by 2 via `inc si /
    inc si`** — the same inc-chain optimisation as
    integer `+= 2` ([[batch-388-arr-or-incpair]]),
    applied to pointer arithmetic via the
    sizeof(int)=2 stride.
  - **`while (n--)`** lowers to `mov ax, dx / dec
    dx / or ax, ax / jne body` — the postfix
    decrement saves the *old* value of n into AX
    before decrementing, then tests AX. This
    materialises the "post" semantics correctly.
- `1552`: confirms global-from-global binop uses the
  memory-operand form. `a + b` where both are
  globals lowers to `mov ax, [_a] / add ax, [_b]`
  with the `0x03 06 disp16` form (`add r16, r/m16`,
  disp16 direct), saving an extra `mov ax,[_b] /
  add ax,ax` round-trip. Two LEDATA FIXUPPs (one
  per global) but only one `add` instruction.

So the enregistration-disqualifier list is now
complete:
- forced-OUT-of-register: use-count<2, `&` taken,
  `volatile`
- forced-INTO-register: `register` keyword (BCC
  honours it)

## `volatile` blocks enregistration; `5+3` global init folds at compile time

Fixtures `1547` (`dbl(a + b)` — binop result passed
as arg), `1548` (`volatile int x; x = x + 1`), and
`1549` (`int g = 5 + 3;`) all pass on the first
capture.

- `1547`: confirms the binop → push fast path:
  `mov ax,[bp-2] / add ax,[bp-4] / push ax / call /
  pop cx`. The `add ax,...` leaves the result in AX
  ready for `push ax` — no intermediate stack
  storage. `a` and `b` are single-use locals
  (1 use after init in the `a+b`), so they stay on
  stack.
- `1548` (**finding**): **`volatile` forces a local
  to stay in memory** regardless of use count.
  Despite `x = x + 1; return x` being 2 syntactic
  uses (would normally enregister `x` into SI),
  BCC emits: `mov ax,[bp-2] / inc ax / mov [bp-2],
  ax / mov ax,[bp-2]` — re-loading from memory even
  immediately after the store. So `volatile` is a
  third constraint that forces stack residence,
  alongside (1) use-count < 2 and (2) address-taken.
- `1549`: confirms compile-time arithmetic folding
  for global initialisers — `int g = 5 + 3;` emits
  the data byte sequence `08 00` (i.e. 8) directly
  in `_DATA`. The expression `5 + 3` is fully
  evaluated by the parser/AST layer before reaching
  codegen.

Combined "spill to memory" conditions for locals:
1. Use count < 2 after declaration.
2. Address taken (`&local` appears anywhere).
3. Declared `volatile`.
Any one of these forces the local into a stack slot.

## SHR/DIV stay word, SHL by 8 still byte (cl=8)

Fixtures `1544` (`(char)(a >> 4)`), `1545` (`(char)(a
/ b)`), and `1546` (`(char)(a << 8)`) finalise the
narrowing-cast propagation table.

- `1544`: SHR does **NOT** propagate. `(char)(a >>
  4)` lowers to **word-width** `mov ax,[bp-2] / mov
  cl,4 / sar ax,cl / cbw`. Correct because the low
  byte of `a >> 4` depends on the *high byte* of
  `a` (the high nibble shifts down into the low
  byte's high nibble), so byte-form `sar al, 4`
  would give a different result.
- `1545`: DIV does **NOT** propagate. `(char)(a /
  b)` lowers to **word-width** `mov ax,[bp-2] / cwd
  / idiv word [bp-4] / cbw`. Division isn't closed
  mod 2^k, and `idiv r/m8` takes AL/AH as dividend
  (with AX being the dividend in word form) — BCC
  always uses the word form under cast.
- `1546`: SHL **does** propagate even for K=8 (and
  presumably any K). `(char)(a << 8)` lowers to
  byte-form `mov al,[bp-2] / mov cl,8 / shl al,cl /
  cbw`. On 8086, `shl r/m8, cl` with cl=8 fully
  clears the byte (count is not masked to 5 bits on
  8086), giving 0 — same as `(low byte of (a <<
  8))` which is also 0. So even for "obviously
  pointless" shifts BCC still emits byte form when
  there's a narrowing cast.

Definitive `(char)(a op b)` propagation table:
| Op  | Byte? | Reason |
|-----|-------|--------|
| ADD | yes   | carry only goes left |
| SUB | yes   | borrow only goes left |
| AND | yes   | bitwise, no cross-bit interaction |
| OR  | yes   | bitwise, no cross-bit interaction |
| XOR | yes   | bitwise, no cross-bit interaction |
| SHL | yes   | high bits exit the byte; correct for any K |
| SHR | **no**| high byte feeds into low byte's high bits |
| DIV | **no**| not closed mod 2^k; AX:DX form needed |
| MOD | **no**| same as DIV |
| MUL | **no** (despite math allowing) | BCC excludes |

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
