# Declarations and storage layout

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

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

## Global `g++` / `g--` statement

Fixture `512` (`int g; g++; g++; return g;`) — `emit_update_in_
place` previously handled only long globals via the `add/adc 1`
pair. Plain int (and char) globals now emit the single
memory-direct `inc word ptr DGROUP:_g` (or `dec` / `byte ptr`
for char). Two new IR variants — `IncGroupSym` and `DecGroupSym`
— encode the `FF 06 lo hi` and `FF 0E lo hi` forms (Grp5 /0 INC
and /1 DEC, ModR/M r/m=110 → `[disp16]`).

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

