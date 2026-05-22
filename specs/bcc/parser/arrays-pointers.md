# Arrays and pointers

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

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

## NULL pointer init

Fixture `533` (`int *g = 0;`) — global pointer initialized to a
null integer constant. The existing scalar-global-init path
handles this directly: codegen emits `dw 0` for the 2-byte
slot. No special-case needed because pointer types have the same
2-byte width as int.

## `++a[K]` peephole

Fixture `547` (`int a[3]; ... ++a[1];`) — `emit_array_compound_
assign` now folds K=1 add/sub into a single `inc|dec <width>
ptr [bp-N]` instruction (1 byte saved vs. `add mem, 1`). A new
tasm IR variant `IncBpRel` / `DecBpRel` encodes `FF 46|4E dd`
(Grp5 /0 or /1 with mod=01 r/m=110 → `[bp]+disp8`). The same
peephole was already in place for register-resident bare-ident
locals; this extends it to memory-direct stack array elements.

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

## `*p = v` — non-constant RHS for int/char pointees

Fixture `595` (`int x; int *p; p = &x; *p = *p + 1;`) — the
`*p = v` path on a register-resident pointer previously
required a constant RHS. Extended the path: when the RHS isn't
const-foldable, `emit_expr_to_ax(value)` materializes the
value in AX/AL, then `mov <width> ptr [<reg>], ax/al` stores
it. A new tasm IR variant `MovSiPtrReg16` encodes `mov word
ptr [si], <reg16>` as `89 (mod=00 reg=<src> r/m=100)`; only
the immediate form was previously supported.

## `while (x--)` — postdec as boolean condition

Fixture `619` — `emit_zero_test` previously handled `Ident`,
`AssignExpr`, and `Call`. Added a `Post`-update arm that
materializes the value-then-side-effect sequence via
`emit_expr_to_ax` and follows with `or ax, ax`. BCC's shape
for `x--` in a boolean context (with `x` in SI) is `mov ax,
si; dec si; or ax, ax` — exactly what the existing postdec
lowering produces when its result is used.

## `add ax, word ptr [di]` — second-pointer dereference

Fixture `625` (`int *p; int *q; ... return *p + *q;`) — BCC
enregisters the two pointer locals into SI and DI; the
sum lowers to `mov ax, [si]; add ax, [di]`. Our tasm
previously only had `AddAxFromSiPtr` (`03 04`). Added the DI
companion `AddAxFromDiPtr` (`03 05`, ModR/M 05 = mod=00
reg=AX r/m=101 ([DI])) and its parser entry.

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

## `while (*p)` — deref through reg-pointer as boolean

Fixture `636` (`char *p; while (*p) { n++; p++; }`) —
`emit_zero_test` panicked because the condition is `Deref(Ident
p)`, not bare `Ident`. BCC's pattern with `p` enregistered in SI
is `cmp byte ptr [si], 0; jne ...` directly (no AX round-trip).
Added a `Deref(Ident reg-pointer)` arm to `emit_zero_test` that
emits `cmp <width> ptr [<reg>], 0` with the width from the
pointee. New tasm IR variant `CmpByteSiPtrImm8` encodes the
byte form (`80 3C ii`).

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

## `and si, word ptr [bp+N]` — generic AND reg-vs-stack

Fixture `655` (`x &= y` with x in SI, y at `[bp-2]`) — tasm
had `AndAxBpRel` and `AndDxBpRel` but no SI/etc. variant.
Added the generic `AndReg16BpRel` IR variant (`23 (mod=01
reg=<r> r/m=110) dd`) — sibling of the batch-110
`CmpReg16BpRel`. AX keeps its dedicated variant.

## Args > 127B offset use `/86 disp16`; `(char*)p + 4` is byte arith; `int (*p)[3]` strides by row; `(*p)++` = `inc word [si]`; ptr casts no-op

Fixtures `2327`-`2332` cover encoding scales,
pointer arithmetic kinds, and casts.

- `2327` (**many args needing disp16**): fn with
  64 int args. ggg (59th arg) at offset +120
  fits disp8, but lll (64th arg) at offset +130
  exceeds it:
  ```
  ; In many(... ggg, ..., lll):
  8b 46 78               ; mov ax, [bp+0x78]  (ggg, disp8)
  03 86 82 00            ; add ax, [bp+0x82]  (lll, disp16)
  ```
  ModR/M `/86 disp16` (4-byte form) for the
  disp16 access; `/46 disp8` (3-byte) for ggg.
- `2328` (**pointer cast int arith**):
  `(int *)((char *)p + 4)` does **byte
  arithmetic** (char* doesn't scale):
  ```
  mov ax, [p]
  add ax, 4              ; raw byte add — char* arith
  mov si, ax              ; cast back to int*
  mov ax, [si]            ; *q = a[2]
  ```
  For int*, +N scales by sizeof; for char*, +N is
  literal bytes. The cast is a type-only change.
- `2329` (**`int (*p)[3]` row-stride pointer**):
  p points to an array-of-3-ints. `p + 1`
  advances by sizeof(int[3]) = 6 bytes:
  ```
  mov si, mat                ; si = &mat[0]
  mov ax, [si+2]             ; (*p)[1] = mat[0][1]
  add ax, [si+10]            ; (*(p+1))[2] = mat[1][2]
  add ax, [si+12]            ; (*(p+2))[0] = mat[2][0]
  ```
  Stride embedded in compile-time offsets.
- `2330` (**static vs auto init cost**):
  ```
  ; static int counter = 100; — initialized ONCE at startup
  _DATA: [64 00]               ; the 100 sits here
  
  ; In sm():
  inc word [counter_addr]     ; ff 06 00 00 (FIXUPP)
  mov ax, [counter_addr]
  
  ; int counter = 100; — initialized EACH call:
  mov si, 100                  ; be 64 00 (runtime!)
  inc si
  ```
  Static = zero per-call init cost; auto = 3 byte
  init each call.
- `2331` (**`(*p)++` and `++*p` collapse to `inc
  word [si]`**):
  ```
  mov si, [p]
  inc word [si]              ; ff 04 — (*p)++
  inc word [si]              ; ff 04 — ++*p
  ```
  When the value isn't used, pre/post difference
  disappears.
- `2332` (**fn ptr cast through void* no-op**):
  ```
  ; fp = offset(two);    ; just 2 bytes (near ptr)
  ; vp = (void*)fp;       ; copy same 2 bytes
  ; fp2 = (...)(vp);     ; copy back same 2 bytes
  ; fp2();
  
  c7 46 fe 00 00            ; fp = FIXUPP
  mov ax, [fp]
  mov [vp], ax              ; just copy
  mov ax, [vp]
  mov [fp2], ax             ; just copy
  ff 56 fa                   ; call near [fp2]
  ```
  All casts are pure type system; same bit pattern
  flows through.

**Pointer arithmetic scaling table**:
| Pointer type | `p + N` adds | Sizeof(*p) |
|--------------|--------------|------------|
| `char *` | N bytes | 1 |
| `short *` | N × 2 bytes | 2 |
| `int *` | N × 2 bytes | 2 |
| `long *` | N × 4 bytes | 4 |
| `float *` | N × 4 bytes | 4 |
| `double *` | N × 8 bytes | 8 |
| `void *` | N bytes (extension) | 1 |
| `T (*)[K]` (ptr-to-array) | N × K × sizeof(T) | K × sizeof(T) |
| `T **` | N × 2 bytes (near) | 2 |
| Struct ptr | N × sizeof(struct) | sizeof(struct) |

**ModR/M displacement threshold (refined)**:
- offset in [-128, 127]: disp8 form `/46 disp8` (3B)
- offset outside: disp16 form `/86 disp16` (4B)
- Per-access decision; BCC picks per ModR/M

**Type-cast codegen impact** (pointer-related):
| Cast | Codegen |
|------|---------|
| Same-size T1* → T2* | No-op (pure type) |
| char* → int* (read) | Same |
| int* → char* (read) | Same |
| Function ptr → void* | No-op (same offset) |
| void* → function ptr | No-op (same offset) |
| Cross-model conversion | (e.g., far-to-near may need segment handling) |

**Static vs auto initializer costs (final)**:
| Storage | Where init lives | Per-call cost |
|---------|------------------|---------------|
| `static int x = N` | `_DATA` template | 0 bytes |
| `static int x` (uninit) | `_BSS` zeros | 0 bytes |
| `int x = N` (auto) | Inlined in prologue | ~5 bytes/var |
| `int x` (auto, uninit) | (no init code) | 0 bytes |

For the Rust reimplementation:
- Track arg/local offsets; emit disp8 vs disp16
  per access.
- Scale ptr arith by sizeof of pointee.
- Casts on pointers: type system only, no
  codegen.
- Static init: emit `_DATA` template once;
  auto init: emit `mov` in prologue.

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


## Array of function pointers — fixture `2343`

`int (*fns[3])(int);` declares 3 near function pointers in the stack
frame, occupying 2 bytes each = 6 bytes total. The array is local so
elements live in BP-relative slots. Initialization is three
`mov word ptr [bp-N], imm16` stores with FIXUPP records resolving each
function address.

```
; main prologue
55 8b ec 83 ec 06       ; sub sp, 6  (3 near ptrs)
c7 46 fa 00 00          ; mov [bp-6], offset add1   ← FIXUPP'd
c7 46 fc 0b 00          ; mov [bp-4], offset add2   ← FIXUPP'd
c7 46 fe 17 00          ; mov [bp-2], offset add3   ← FIXUPP'd
```

Indirect calls through array elements use `call near [bp+disp]`
(`ff 56 disp8`) — the same encoding as any indirect call through a
stack-resident near pointer:

```
b8 0a 00 50             ; push 10
ff 56 fa                ; call near [bp-6]   ← fns[0](10)
59                      ; pop cx             ← cdecl cleanup of 1 arg
```

So the codegen path for `fns[K](arg)` is just: compute the slot
displacement at compile time (constant-index subscript), then emit a
single `ff 56 disp8`. No `mov ax,[bp-6] / call ax` round-trip — the
ModR/M form already supports indirect-call-through-memory.

## `char *names[] = {"alice", "bob", "carol"};` — string-literal pointer array (fixture `2345`)

A file-scope `char *names[]` initialized from string literals allocates
two segments worth of data:

1. **Pointer slots in `_DATA`**: 3 near pointers × 2 bytes = 6 bytes,
   one per array element. Each slot's value is the offset of its
   string in `_DATA` — resolved via three FIXUPP records.
2. **The string literal bodies** also in `_DATA`, NUL-terminated.

Element access `names[K][0]` lowers to:

```
8b 1e 00 00             ; mov bx, [_names+K*2]  (FIXUPP'd)
8a 07                   ; mov al, [bx]          ← *names[K]
98                      ; cbw                   ← widen to int
```

So the indirection is explicit: load the pointer, then deref. There
is no fused `mov al, [_strN]` shortcut even though the pointer is a
constant — the compiler treats `names[K]` as an ordinary subscript
that produces a pointer rvalue at runtime.

Chained `+` of the three character loads goes through the standard
push/pop accumulator pattern (each cbw'd char gets pushed, then
popped+added to the next).

## 2D array with runtime-variable indices — fixture `2346`

`int m[3][4]` with both indices variable forces a runtime stride
computation. The address `&m[i][j]` is `&m[0][0] + i*8 + j*2` (outer
stride 4 elems × 2 bytes = 8, inner stride 2). BCC computes this with
three `shl bx, 1` for the ×8 stride:

```
8b de                   ; mov bx, i
d1 e3 d1 e3 d1 e3       ; shl bx, 1 ×3  (= ×8 outer stride)
8b c7                   ; mov ax, j
d1 e0                   ; shl ax, 1     (= ×2 inner stride)
03 d8                   ; add bx, ax
8d 46 e8                ; lea ax, [bp-24]  ← &m[0][0]
03 d8                   ; add bx, ax    ← bx = full element address
c7 07 4d 00             ; mov [bx], 77
```

Confirms the **N=3 shift-unroll threshold**: `shl bx, 1 / shl bx, 1 /
shl bx, 1` (6 bytes) beats `mov cl, 3 / shl bx, cl` (5 bytes) only by
1 byte but BCC consistently prefers the unrolled form at N=3. (N=4
would tip into `cl`-form.)

The same address-compute sequence is emitted **twice** — once for the
write `m[i][j] = 77`, once for the read `return m[i][j]`. No CSE.
Each subscript expression is lowered independently.

## `char *` pointer subtraction — fixture `2347`

For `char *p - char *s` BCC emits a direct word subtract with no
divide, since `sizeof(char) = 1`:

```
8d 46 fa                ; lea ax, [bp-6]   ← &s[0]
50                      ; push ax
8b c6                   ; mov ax, si       ← p (enregistered)
5a                      ; pop dx           ← &s[0]
2b c2                   ; sub ax, dx       ← p - s
```

Contrast with `int *` subtraction (fixture `2241`), which inserts an
`idiv` by `sizeof(int) = 2` after the subtract. The compiler routes
on the pointee type at compile time.

The `while (*p) p++;` walk-to-NUL uses the classic test-at-top
template — `jmp` to the test first, then loop body, then test:

```
8d 46 fa 8b f0          ; p = &s[0]
eb 01                   ; jmp test
                        ; loop:
46                      ; inc si           ← p++
                        ; test:
80 3c 00                ; cmp byte ptr [si], 0
75 fa                   ; jne loop (-6)
```

`80 3c 00` is `cmp byte ptr [si], 0` (`mod=00 r/m=100` = `[si]`,
no displacement). Compare-with-zero against a byte through a register
pointer.

## `sizeof(array)` arithmetic folded at compile time (fixture `2375`)

`return sizeof(a) - sizeof(p);` where `a` is `int a[5]` and `p` is
`int *`. BCC folds the entire expression at parse time:

```
b8 08 00                ; mov ax, 8   ← (5*2) - 2 = 8, computed at parse
```

No runtime sizeof emit. Confirms `sizeof` is a constant expression
and integer arithmetic between sizeof results is constant-folded.
This includes when one operand is `sizeof(array_object)` (= total
bytes = 10) and the other is `sizeof(pointer)` (= 2). The C90 rule
that arrays don't decay inside `sizeof` is also implicitly confirmed
— if `sizeof(a)` had decayed to `sizeof(int*)`, the result would be 0.

## Negative array subscript — signed disp8 in ModR/M (fixture `2377`)

`p[-1]` with `p` a pointer compiles to a single load with a
**negative-displacement** ModR/M:

```
8b 44 fe        ; mov ax, [si + (-2)]   ← disp8 = 0xFE = -2 signed
```

Encoding details:
- `8b` = `mov r16, r/m16`
- `44` = ModR/M `mod=01, reg=000 (ax), rm=100 ([si]+disp8)`
- `fe` = disp8 (= -2 in signed two's complement)

So `p[K]` for a constant negative `K` collapses to disp8 access — no
separate `sub` instruction needed for the negative offset. The
compile-time stride computation (`K * sizeof(*p) = -1 * 2 = -2`)
produces a value that fits in the signed-byte disp8 range
(-128..+127), letting the access fold into one instruction.

For larger negative offsets that don't fit in disp8, BCC would
presumably emit disp16 (`8b 84 NN NN`) — outside this fixture's
scope.

## `(c ? a : b)[i]` — ternary of arrays returns a pointer (fixture `2379`)

Conditional expressions over arrays exploit array-to-pointer decay:
the ternary doesn't return an array, it returns a pointer to the
chosen array. Both branches compile a `lea` of the array's stack
address.

```c
return (c ? a : b)[1];
```

```
83 7e f2 00             ; cmp [c], 0
74 05                   ; je b_branch
8d 46 fa                ; lea ax, [bp-6]   ← &a[0]
eb 03                   ; jmp end_cond
                        ; b_branch:
8d 46 f4                ; lea ax, [bp-12]  ← &b[0]
                        ; end_cond:
8b d8                   ; mov bx, ax       ← BX = chosen ptr
8b 47 02                ; mov ax, [bx + 2] ← [1] = 1 * sizeof(int)
```

Each branch computes the array's address via `lea` and lands it in
AX. After the ternary join, the result is moved to BX and the
subscript adds the constant `+2` for `[1]`. Confirms: array-typed
expressions decay to pointers when not the operand of `sizeof` or
`&`, including inside ternary operands.

## Type punning via `*((char *)&i)` — accesses low byte (fixture `2430`)

`*((char *)&i)` for an `int i` accesses the **low byte** of `i` —
type punning through pointer cast works correctly:

```c
int i;
char c;
i = 0x12AB;
c = *((char *)&i);   // c = 0xAB (low byte)
```

```
c7 46 fe ab 12          ; i = 0x12AB at [bp-2]
8a 46 fe                ; mov al, [bp-2]    ← read LOW byte
88 46 fd                ; c = al at [bp-3]
8a 46 fd                ; mov al, c
98                      ; cbw (signed widen for return)
```

The cast `(char *)&i` carries no codegen — the `&i` produces a
pointer value, and the cast retypes it without any conversion
bytes. Then `*` derefs as byte via `mov al, [bp+disp]`.

Confirms (once more) **little-endian byte ordering**: `i = 0x12AB`
stores 0xAB at the low address (i's first byte) and 0x12 at the high
address. The char-ptr cast access reads the low byte.

This is the standard portable way to extract the low byte of a
multi-byte value in C90 code — works thanks to BCC's permissive
type-system handling at cast sites and the platform's little-endian
storage.

## `(*p)--` — direct memory decrement via pointer (fixture `2449`)

`r = (*p)--;` lowers to a direct memory-decrement, no register
round-trip:

```c
int x = 10;
int *p = &x;
r = (*p)--;     // r = 10, x becomes 9
```

```
8b 04                   ; mov ax, [si]    ← read *p into AX (old value for r)
ff 0c                   ; dec word ptr [si]  ← decrement *p in memory directly
89 46 fc                ; r = ax           (the OLD value)
```

Encoding `ff 0c`:
- `ff` = single-operand op
- `0c` = ModR/M `mod=00 reg=001 rm=100` = `/1 = dec` on `r/m=100 = [si]`

So `dec word ptr [si]` modifies `*p` in place without load-modify-
store. The old value was already captured to AX before the
decrement, providing the post-decrement rvalue.

Compares with `r = (*p)++` which would use `inc word ptr [si]` (the
`/0` subop) — same template, different opcode bit.

This memory-direct form parallels the documented `g++` /
`g--` peephole for globals; here it works for the
`pointer-dereference-then-postdec` pattern too. The key is that both
sides of the modify (read + write) target the same memory operand,
so BCC peepholes through the `mov [src], reg / op [src]` pair.

## a[i++] = K with i just initialized — NOT constant-folded

Fixture `2499-postfix-inc-subscript-obj`:

```c
int a[4];
int main(void) {
  int i;
  i = 0;
  a[i++] = 7;
  return i;
}
```

```
55 8b ec 56           prologue + push si
33 f6                 xor si, si           ; i = 0
8b de                 mov bx, si           ; bx = i (for indexing)
d1 e3                 shl bx, 1            ; bx = i * sizeof(int) = i*2
c7 87 00 00 07 00     mov word [bx + _a], 7    ; a[i] = 7 (FIXUPP for _a)
46                    inc si               ; i++
8b c6                 mov ax, si           ; return i
eb 00 5e 5d c3        epilogue
```

Findings:
- BCC does **NOT** constant-fold the postfix-subscript even when the
  index variable was just initialized to 0. `i` lives in si; bx is a
  scratch copy that gets shifted into a byte offset. The store
  literally indexes `_a` at runtime.
- ModR/M form `87` is mod 10, r/m 111 → `[bx + disp16]`; the disp16
  is the FIXUPP for `_a` at link time. So all `array[var-index]`
  stores on a global use this exact instruction shape.
- Postfix increment uses single `inc si` (1 byte) AFTER the store
  side-effect completes — sequence point honored.
- Note: si holds `i` (the live value); bx holds the *byte-scaled
  index*. BCC always shifts a fresh copy rather than mutating the
  user variable.


## Initialized array of struct — single contiguous LEDATA, no padding

Fixture `2502-array-of-structs-init-obj`:

```c
struct Point { int x; int y; };
struct Point pts[2] = { { 1, 2 }, { 3, 4 } };
int main(void) {
  return pts[1].y;
}
```

Init bytes in `_DATA`:
```
01 00 02 00 03 00 04 00     ; pts[0].x, pts[0].y, pts[1].x, pts[1].y
```

Main body:
```
55 8b ec              prologue
a1 06 00              mov ax, [_pts + 6]   ; pts[1].y (FIXUPP for _pts disp16=6)
eb 00 5d c3           epilogue
```

Findings:
- The initializer is a SINGLE contiguous LEDATA emission — `1, 2, 3, 4`
  packed back-to-back as little-endian int16 with NO struct-boundary
  padding (struct is 4 bytes = 2 ints exactly).
- `pts[1].y` folds at compile time to the constant byte offset `6`:
  pts[1] starts at +4, .y is the second int (+2), total +6.
  Emitted as `mov ax, moffs16` (`a1` opcode), the disp16 carrying a
  FIXUPP for `_pts + 6`. No subscript arithmetic at runtime — no LEA,
  no shifts.
- Constant-initialized data goes in `_DATA`, never `_BSS`.


## Pointer subtraction — sub + idiv by element size (always signed)

Fixture `2506-pointer-subtract-obj`:

```c
int a[10];
int main(void) {
  int *p, *q;
  p = &a[7];  /* _a + 14 */
  q = &a[2];  /* _a + 4 */
  return p - q;
}
```

```
55 8b ec 83 ec 04     prologue + 4B locals (2 ptrs)
c7 46 fe 0e 00        p = _a+14 (FIXUPP _a, disp16=14)
c7 46 fc 04 00        q = _a+4  (FIXUPP _a, disp16=4)
8b 46 fe              ax = p
2b 46 fc              sub ax, q                ; byte difference
bb 02 00              mov bx, 2                ; sizeof(int)
99                    cwd                      ; sign-extend ax → dx:ax
f7 fb                 idiv bx                  ; signed divide
eb 00 8b e5 5d c3     epilogue
```

Findings:
- Pointer subtraction `p - q` for `int *` compiles as
  `sub ax, q; mov bx, 2; cwd; idiv bx`. **Always signed**: `cwd` +
  `idiv`, never `sar` even though `sizeof(int) = 2` is a power of
  two. This matches the C semantics that `ptrdiff_t` is signed.
- The element size (2) is loaded into bx as an imm16 each time —
  no register reuse from earlier code.
- `&a[K]` for constant K is fully folded at compile time: the
  byte offset (K * sizeof(*p)) goes directly into the immediate,
  with a FIXUPP for the array's base symbol. No runtime `lea`.
- `mov word [bp-disp], imm16` instructions CAN carry FIXUPPs on
  their disp16 immediate field — confirmed via two consecutive
  `c7 46 fe 0e 00` and `c7 46 fc 04 00` here, each fixupp'd to
  `_a` with the disp baked in.


## 2D array with constant subscripts — fully folded byte offset

Fixture `2512-2d-array-access-obj`:

```c
int m[3][4];
int main(void) {
  m[1][2] = 7;
  return m[1][2];
}
```

```
55 8b ec                          prologue
c7 06 0c 00 07 00                 mov word [_m + 12], 7   ; FIXUPP _m
a1 0c 00                          mov ax, [_m + 12]       ; FIXUPP _m
eb 00 5d c3                       epilogue
```

Findings:
- `m[1][2]` with both indices constant folds **at compile time**
  to byte offset = row × col-count × sizeof(int) + col × sizeof(int)
  = 1 × 4 × 2 + 2 × 2 = 12.
- Emitted as a single store/load to `[_m + 12]` with FIXUPP for
  `_m` — same shape as `obj.i.v` flattening.
- No runtime row-stride multiply, no shift. The 2D-ness is purely
  a parser-side bookkeeping concern; codegen sees only an offset.
- This generalizes to N-dimensional arrays with constant indices:
  all collapse to a single `disp16 + FIXUPP` regardless of rank.


## `*p++` for int* — uses `inc reg; inc reg` (NOT `add reg, 2`)

Fixture `2518-deref-post-inc-obj`:

```c
int a[3];
int main(void) {
  int *p;
  int v;
  p = a;
  v = *p++;
  return v;
}
```

```
55 8b ec 4c 4c                  prologue + 2B local for v
56                              push si
be 00 00                        mov si, 0          ; p = _a (FIXUPP _a)
8b 04                           mov ax, [si]       ; *p
89 46 fe                        mov [bp-2], ax     ; v = *p
46                              inc si             ; p++ (byte 1 of 2)
46                              inc si             ; p++ (byte 2 of 2)
8b 46 fe                        mov ax, v
eb 00 5e 8b e5 5d c3            epilogue
```

Findings:
- `p++` for an `int*` (`sizeof(int) = 2`) emits as **two `inc reg`
  instructions** (`46 46` = 2 bytes), NOT `add reg, 2` (`83 c6 02`
  = 3 bytes). One-byte savings via the inc peephole.
- The "++" comes AFTER the deref-and-store, respecting the postfix
  semantic — exactly the source order.
- Pointer `p` lives in si (single use beyond init), but the source
  variable `v` is on the stack because it's the address-taken
  target of an assignment. Even with only-one-use, the
  expression-result `v` is forced to memory before being returned —
  no register coalescing across the assignment.
- Confirms a pattern: any `+= 2` on an int-typed register that's a
  pointer becomes `inc; inc` rather than `add reg, 2`. To probe:
  does `p++` for `long*` (sizeof=4) become `inc;inc;inc;inc`, or
  switch to `add reg, 4`?


## `a[i << K]` — explicit shift NOT fused with sizeof scaling

Fixture `2530-array-idx-shifted-obj`:

```c
int a[10];
int main(void) {
  int i;
  i = 3;
  return a[i << 1];
}
```

```
55 8b ec 4c 4c                prologue + 2B local
c7 46 fe 03 00                i = 3
8b 5e fe                      mov bx, i
d1 e3                         shl bx, 1                ; explicit `i << 1`
d1 e3                         shl bx, 1                ; sizeof(int) = 2 scaling
8b 87 00 00                   mov ax, [bx + _a]        ; FIXUPP _a
eb 00 8b e5 5d c3             epilogue
```

Findings:
- `a[i << 1]` for `int a[]` (sizeof=2) emits **two separate `shl bx, 1`**
  — one for the user-written `i << 1`, one for the implicit sizeof
  scaling. BCC does **NOT fuse** them into a single `shl bx, 2` or
  `shl bx, cl` with cl=2.
- This means the array-index codegen is purely compositional:
  - Compute the index rvalue (whatever its expression form is)
  - Then apply the sizeof-scale shift
- For the shift-by-2 final result, the unroll-vs-cl rule still
  picks the unroll path (2 single-bit shifts under N≤3).
- ModR/M `87` = mod 10, r/m 111 → `[bx + disp16]`; disp16 carries
  FIXUPP for `_a`.


## 2D array initializer — single flat LEDATA, row-major

Fixture `2535-2d-array-init-obj`:

```c
int m[2][3] = { { 1, 2, 3 }, { 4, 5, 6 } };
int main(void) {
  return m[1][2];
}
```

`_DATA` bytes for `_m`:
```
01 00 02 00 03 00 04 00 05 00 06 00     ; row 0: 1,2,3 then row 1: 4,5,6
```

Main body:
```
55 8b ec                       prologue
a1 0a 00                       mov ax, [_m + 10]   ; m[1][2]
eb 00 5d c3                    epilogue
```

Findings:
- 2D array `int m[2][3]` initializer flattens to a **single
  contiguous 12-byte LEDATA** in `_DATA`. Row-major order (row 0
  first, then row 1). NO separators or padding between rows.
- `m[1][2]` constant subscript folds to byte offset 10: row 1
  × (3 cols × 2B) + col 2 × 2B = 6 + 4 = 10.
- Single `mov ax, moffs16` (`a1` opcode, 3 bytes) — no
  runtime row-stride multiplication.
- Confirms the rule from `2512`: N-dimensional arrays with all
  constant subscripts collapse to a single byte offset + FIXUPP.
- The "shape" of the source initializer (nested braces) carries
  no runtime cost — same byte sequence as `{1,2,3,4,5,6}` would
  produce.


## `*p++` for char* — single `inc reg` (1 byte)

Fixture `2557-char-ptr-postinc-obj`:

```c
char buf[5];
int main(void) {
  char *p;
  char c;
  p = buf;
  c = *p++;
  return c;
}
```

```
55 8b ec 4c 4c                 prologue + 2B local
56                             push si
be 00 00                       mov si, &buf (FIXUPP)
8a 04                          mov al, [si]          ; *p (byte load)
88 46 ff                       mov [bp-1], al        ; c = *p (byte at [bp-1])
46                             inc si                ; p++ (sizeof char = 1)
8a 46 ff                       mov al, c
98                             cbw
eb 00 5e 8b e5 5d c3           epilogue
```

Findings:
- **`p++` for `char*` (sizeof=1) emits a SINGLE `inc reg` (1 byte)**.
- Inc-vs-add table now complete:

| sizeof | bytes for `p++`     | instruction(s)         |
|--------|---------------------|------------------------|
| 1      | 1B                  | `inc reg`              |
| 2      | 2B                  | `inc reg; inc reg`     |
| 4      | 3B                  | `add reg, 4`           |

  Threshold: BCC uses inc-chain for delta ≤ 2, switches to `add reg, imm8`
  at delta = 4. (delta = 3 to be probed.)
- **Char local `c` is stored at `[bp-1]`** (odd-byte address), filling
  the otherwise-padded slot of the 2-byte reserve. So 1-byte chars at
  function scope DO use the odd half of the even-padded frame.
- Byte store uses `88 46 ff` (`mov [bp-1], al`, opcode `88` = byte store).
- Char return path: `mov al, c; cbw` for int-context return.


## `int **pp` — chained dereference via single register

Fixture `2570-ptr-to-ptr-obj`:

```c
int x = 42;
int *p = &x;
int **pp = &p;
int main(void) {
  return **pp;
}
```

`_DATA` (6 bytes, declaration order):
```
2a 00     ; x = 42 at offset 0
00 00     ; p (= &x) at offset 2, FIXUPP _x
02 00     ; pp (= &p) at offset 4, FIXUPP _p
```

Main body:
```
55 8b ec                       prologue
8b 1e 04 00                    mov bx, [_pp]          ; bx = pp's value (= addr of p)
8b 1f                          mov bx, [bx]           ; bx = *pp (= addr of x)
8b 07                          mov ax, [bx]           ; ax = **pp (= 42)
eb 00 5d c3                    epilogue
```

Findings:
- `**pp` produces **three sequential loads**, all chained through
  bx. Each `mov bx, [bx]` (`8b 1f` = mod 00 r/m 111 = `[bx]`) is
  the canonical deref step.
- Final deref lands in ax for the return value.
- No optimization for known-at-compile-time chains (pp, p, x all
  initialized to known addresses) — BCC dereferences all three at
  runtime.
- Globals laid out in **source declaration order** in `_DATA`:
  x@0, p@2, pp@4.
- The init expressions `&x`, `&p` are FIXUPP'd at link time —
  these are inter-segment relocations within `_DATA`.


## Negative subscript `p[-1]` — uses signed disp8 in ModR/M

Fixture `2584-neg-arr-idx-obj`:

```c
int a[5];
int main(void) {
  int *p;
  p = &a[2];          /* = _a + 4 */
  return p[-1] + p[0];
}
```

```
55 8b ec 56                    prologue + push si
be 04 00                       mov si, _a + 4 (FIXUPP)    ; p = &a[2]
8b 44 fe                       mov ax, [si-2]             ; p[-1]
03 04                          add ax, [si]               ; + p[0]
eb 00 5e 5d c3                 epilogue
```

Findings:
- **Negative subscript on a pointer folds to a signed disp8 in
  ModR/M**: `p[-1]` for `int*` (sizeof=2) → `[si + 0xfe]` where
  `0xfe` is `-2` as a signed byte.
- ModR/M encoding `44` = mod 01, r/m 100 → `[si + disp8]`, then
  the disp8 `0xfe` is interpreted as signed (-2).
- `&a[2]` for const index folds to `_a + 4` with FIXUPP — no
  runtime address arithmetic, even when the offset is into the
  middle of the array.
- The two-fetch sum `p[-1] + p[0]` uses the AX-accumulator pattern
  with direct memory operands on the second fetch (`add ax,
  [si]`).
- This confirms: pointer arithmetic with constant offsets always
  folds to a baseregister+disp form, even for negative offsets.


## `*p++ = K` for char* — `mov byte [si], imm8; inc si` (4 bytes per write)

Fixture `2589-deref-postinc-write-obj`:

```c
char buf[5];
int main(void) {
  char *p;
  p = buf;
  *p++ = 'A';
  *p++ = 'B';
  return 0;
}
```

```
55 8b ec 56                    prologue + push si
be 00 00                       mov si, _buf (FIXUPP)
c6 04 41                       mov byte [si], 'A'
46                             inc si
c6 04 42                       mov byte [si], 'B'
46                             inc si
33 c0                          xor ax, ax
eb 00 5e 5d c3                 epilogue
```

Findings:
- `*p++ = K` for char* compiles as **`c6 04 imm8; 46`** — direct
  byte store at `[si]` followed by single-byte inc.
- ModR/M `04` = mod 00, r/m 100 → `[si]` with no displacement.
- Total: **4 bytes per write-and-advance**. Two consecutive ops
  = 8 bytes — BCC does NOT coalesce them into a single 2-byte
  word store (which would be `c7 04 imm16` = 4 bytes for both
  if the bytes happened to be word-aligned).
- The return `0` uses `xor ax, ax` (2B) per the standard zero-load
  peephole.
- This is the idiomatic "string-building" pattern in C — and BCC
  emits the compact form expected.

