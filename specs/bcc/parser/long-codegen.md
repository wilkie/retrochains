# `long` / `unsigned long` codegen

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

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


## `long *p++` — `add si, 4` (NOT four `inc si`)

Fixture `2521-long-ptr-postinc-obj`:

```c
long a[3];
int main(void) {
  long *p;
  long v;
  p = a;
  v = *p++;
  return (int)v;
}
```

```
55 8b ec                    prologue
83 ec 04                    sub sp, 4              ; 4B local for v (long)
56                          push si                ; p in si
be 00 00                    mov si, 0              ; p = _a (FIXUPP)
8b 44 02                    mov ax, [si+2]         ; HIGH word of *p
8b 14                       mov dx, [si]           ; LOW word of *p
89 46 fe                    mov [bp-2], ax         ; v.HIGH at offset -2
89 56 fc                    mov [bp-4], dx         ; v.LOW at offset -4
83 c6 04                    add si, 4              ; p++ (sizeof(long) = 4)
8b 46 fc                    mov ax, [bp-4]         ; return (int)v = LOW
eb 00 5e 8b e5 5d c3        epilogue
```

Findings:
- **`p++` for `long*` uses `add si, 4` (3 bytes)**, NOT four
  `inc si` (4 bytes). So the inc-vs-add threshold is at +2: BCC
  picks `inc; inc` when delta is exactly 2 (saves 1 byte vs add),
  but uses `add reg, imm8` for +4 and beyond.
- **Long value load through pointer**: BCC issues two independent
  `mov reg, [si+disp]` instructions — high word at [si+2] into AX,
  low word at [si] into DX. The order is **HIGH first into AX, LOW
  second into DX** (suggests an evaluation pattern, not a DX:AX-as-
  pair convention).
- **Long value store to stack**: stores AX (which holds the HIGH
  word) at [bp-2] and DX (LOW word) at [bp-4]. So in memory the
  long at `[bp-4]` has low word at -4, high word at -2 — standard
  little-endian.
- **`(int)v` cast** truncates by loading only the low word of v:
  `mov ax, [bp-4]`. The high half [bp-2] is discarded — no zero-
  extension, no movzx. Cast = address selection.


## `(long)x` for signed int — sign-extend via `cwd` (1 byte)

Fixture `2548-int-to-long-cast-obj`:

```c
long widen(int x) {
  return (long)x;
}
```

```
55 8b ec                       prologue
8b 46 04                       mov ax, x
99                             cwd                ; sign-extend ax → dx:ax
eb 00 5d c3                    epilogue
```

Findings:
- **`(long)x` for signed `int x`** uses **`cwd`** (`99`, 1 byte):
  AX → DX:AX, with DX = 0xFFFF if AX negative, 0x0000 otherwise.
- Long return convention: **DX:AX** (same as struct{long} from
  `2532`). DX = HIGH word, AX = LOW word.
- For **unsigned int → long** cast, BCC would use **`xor dx, dx`**
  (`33 d2`, 2 bytes) to zero-extend. To probe.
- This is the inverse of the truncating `(int)long_v` cast from
  `2521`, which loaded only the low word of the long.
- Cast operator table for primitive widening:

| from → to           | bytes                |
|---------------------|----------------------|
| signed char → int   | `cbw` (1B)           |
| unsigned char → int | `mov ah, 0` (2B)     |
| int → long          | `cwd` (1B)           |
| unsigned int → long | `xor dx, dx` (2B)    |


## `(long)unsigned int` — zero-extend via `xor dx, dx`

Fixture `2549-uint-to-long-cast-obj`:

```c
long widen(unsigned int u) {
  return (long)u;
}
```

```
55 8b ec                       prologue
8b 46 04                       mov ax, u
33 d2                          xor dx, dx          ; zero-extend
eb 00 5d c3                    epilogue
```

Findings:
- **`(long)u` for unsigned int** uses **`xor dx, dx`** (`33 d2`, 2 bytes)
  to clear the high half — zero-extension.
- Confirms the widening cast table:

| from → to            | bytes              |
|----------------------|--------------------|
| signed int → long    | `cwd` (1B)         |
| unsigned int → long  | `xor dx, dx` (2B)  |

- The signedness of the source is the ONLY thing that distinguishes
  these two; BCC tracks signedness through the type system and picks
  the right widening at the cast site.


## Global `long v = -1000000L` — 4 bytes little-endian two's complement

Fixture `2560-long-neg-init-obj`:

```c
long v = -1000000L;
int main(void) {
  return (int)v;
}
```

`_DATA` bytes for `_v`: `c0 bd f0 ff`  (= `0xFFF0BDC0` = -1000000)

Main body:
```
55 8b ec                       prologue
a1 00 00                       mov ax, [_v]      ; LOW word only
eb 00 5d c3                    epilogue
```

Findings:
- Negative long literal emits as **4 bytes little-endian two's-
  complement** in `_DATA`. No special "negate at runtime" sequence —
  fully constant-folded.
- Bytes for `-1000000` (= 0xFFF0BDC0):
  - byte 0 (LOW LOW):  c0
  - byte 1 (LOW HIGH): bd
  - byte 2 (HIGH LOW): f0
  - byte 3 (HIGH HIGH): ff
  Reading as two little-endian words: low word = 0xBDC0,
  high word = 0xFFF0.
- `(int)v` cast TRUNCATES to the low word: a single `mov ax, [_v]`
  loads `0xBDC0` from offset 0. The high word at offset 2 is never
  read.
- This generalizes: any `(int)long_var` cast = moffs16 word load at
  the long's BASE address (which holds the low word in little-endian).


## `(int)(a + b)` for ulong — type-directed eager truncation

Fixture `2569-ulong-add-obj`:

```c
unsigned long a;
unsigned long b;
a = 0x10000UL;
b = 0x20002UL;
return (int)(a + b);
```

```
55 8b ec 83 ec 08              prologue + 8B locals (2 longs × 4B)
                               ; a = 0x10000 → HIGH=1, LOW=0
c7 46 fe 01 00                 [bp-2] = 0x0001    ; a HIGH
c7 46 fc 00 00                 [bp-4] = 0x0000    ; a LOW
                               ; b = 0x20002 → HIGH=2, LOW=2
c7 46 fa 02 00                 [bp-6] = 0x0002    ; b HIGH
c7 46 f8 02 00                 [bp-8] = 0x0002    ; b LOW
                               ; return (int)(a + b)
8b 46 fc                       mov ax, [bp-4]     ; a.LOW
03 46 f8                       add ax, [bp-8]     ; + b.LOW
eb 00 8b e5 5d c3              epilogue
```

Findings:
- **`(int)long_expr` triggers EAGER truncation**: BCC sees the cast
  and only emits the bits of computation needed for the low word.
  Here `a + b` would normally be a 32-bit add (low word add + high
  word adc), but only the low-word add is emitted.
- The high-word `adc` is **NOT** emitted — the high half of the
  result wouldn't be used after the int-cast anyway. This is
  correct: `(int)(a + b) == int(low(a) + low(b))` regardless of
  any carry, because the carry only affects the high word.
- This generalizes: a `(int)` cast over a multi-word arithmetic
  expression EXCISES the high-word work.
- Long-store byte order in memory is unchanged: HIGH word at
  higher address (offset 2 within the long), LOW at offset 0. So
  `[bp-2]` holds the HIGH word, `[bp-4]` the LOW (for a long at
  `[bp-4..-1]`).


## Long signed right shift — runtime helper `N_LXRSH@`

Fixture `2575-long-sar-obj`:

```c
long s = -16L;
s = s >> 2;
return (int)s;
```

```
55 8b ec 83 ec 04              prologue + 4B local (long s)
c7 46 fe ff ff                 [bp-2] = 0xFFFF  (HIGH of -16L)
c7 46 fc f0 ff                 [bp-4] = 0xFFF0  (LOW of -16L)
8b 56 fe                       mov dx, [bp-2]   ; HIGH → dx
8b 46 fc                       mov ax, [bp-4]   ; LOW  → ax
b1 02                          mov cl, 2        ; shift count
e8 00 00                       call N_LXRSH@    ; (EXTDEF FIXUPP)
89 56 fe                       [bp-2] = dx      ; store back HIGH
89 46 fc                       [bp-4] = ax      ; store back LOW
8b 46 fc                       mov ax, [bp-4]   ; (int)s = LOW
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Long shifts use a **runtime library helper**, NOT inline shifts.
  The 8086 lacks a single 32-bit shift instruction; doing it inline
  would require 2× shift + carry-flip per bit (~6 instructions per
  shift). The library function is cheaper after about 2-3 shifts.
- Helper convention:
  - Input: **DX:AX = the long value**, **CL = shift count**
  - Output: **DX:AX = shifted result**
  - Helper preserves bp, si, di (cdecl-conformant)
- Helper names so far observed:
  - `N_LXRSH@` — signed long right shift (this fixture)
  - `N_SCOPY@` — struct/string copy (`2509`, `2526`)
  - Expected siblings: `N_LXLSH@` (left shift), `N_LXRSU@`
    (unsigned right shift), multiply/divide helpers.
- The helper is called WITHOUT pushing args — DX:AX/CL are the
  ABI. This is a **fast-call convention** specific to long-arith
  helpers, distinct from cdecl push-stack convention.
- No cleanup needed after — helper consumes DX:AX/CL and returns
  in DX:AX, no stack pushes.


## Unsigned long right shift — `N_LXURSH@` helper (different from signed)

Fixture `2579-ulong-shr-helper-obj`:

```c
unsigned long u = 0xF0000000UL;
u = u >> 2;
return (int)u;
```

Same call shape as signed long shift (`2575`), but the EXTDEF
symbol is `N_LXURSH@` instead of `N_LXRSH@`:

| operator           | symbol     |
|--------------------|------------|
| signed `long >> N` | `N_LXRSH@` |
| unsigned `long >> N` | `N_LXURSH@` |
| left shift (any)   | likely `N_LXLSH@` — to probe |
| signed `long * long` | `N_LXMUL@` (see below) |

Findings:
- **Helper naming distinguishes signedness via "U" infix**:
  `N_LXRSH@` (signed sar) vs `N_LXURSH@` (unsigned shr). Adding
  one character to the symbol changes the rounding/sign behavior.
- Left shifts are the same for signed and unsigned (both fill
  with zeros on the right) — probably a single `N_LXLSH@` helper.
- ABI matches `2575`: DX:AX = value, CL = count, returns DX:AX.

## Long multiplication — `N_LXMUL@` helper

Fixture `2580-long-mult-obj`:

```c
long a = 1000L;
long b = 2L;
return (int)(a * b);
```

```
55 8b ec 83 ec 08              prologue + 8B locals (2 longs)
c7 46 fe 00 00                 a HIGH = 0
c7 46 fc e8 03                 a LOW = 1000
c7 46 fa 00 00                 b HIGH = 0
c7 46 f8 02 00                 b LOW = 2
8b 4e fe                       cx = a HIGH
8b 5e fc                       bx = a LOW
8b 56 fa                       dx = b HIGH
8b 46 f8                       ax = b LOW
e8 00 00                       call N_LXMUL@   ; (EXTDEF FIXUPP)
                               ; DX:AX = product (low 32 bits)
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Long-multiply helper ABI:
  - **CX:BX = first operand (a)**: CX=HIGH, BX=LOW
  - **DX:AX = second operand (b)**: DX=HIGH, AX=LOW
  - **Return: DX:AX = product (low 32 bits)**; high 32 bits discarded
- All four scratch registers (AX, BX, CX, DX) are consumed by the
  input — caller must spill anything live in those before the call.
- One helper covers both signed and unsigned multiply because the
  low 32 bits of the product are bit-pattern-identical regardless
  of signedness interpretation. (Same as int8086 `imul` vs `mul`
  giving identical low-word results.)
- `(int)(a*b)` after the helper just keeps AX — DX (high half) is
  discarded. No type-directed elision of the helper call itself
  because the helper computes the full product internally.


## Long divide — `N_LDIV@` helper with STACK args, callee-cleans-up via epilogue

Fixture `2585-long-div-obj`:

```c
long a = 1000000L;
long b = 7L;
return (int)(a / b);
```

```
55 8b ec 83 ec 08              prologue + 8B locals (2 longs)
c7 46 fe 0f 00                 a HIGH = 0x000F
c7 46 fc 40 42                 a LOW  = 0x4240
c7 46 fa 00 00                 b HIGH = 0
c7 46 f8 07 00                 b LOW  = 7
ff 76 fa                       push word [bp-6]    ; b HIGH
ff 76 f8                       push word [bp-8]    ; b LOW
ff 76 fe                       push word [bp-2]    ; a HIGH
ff 76 fc                       push word [bp-4]    ; a LOW
e8 00 00                       call N_LDIV@         ; (EXTDEF FIXUPP)
                               ; NO `add sp, 8` here! Epilogue restores.
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Long divide uses **`N_LDIV@`** helper. Note the symbol prefix
  differs from multiply (`N_LXMUL@`) — there's NO consistent
  "LX" infix across all long ops. Possible naming:
  - `N_LXRSH@` / `N_LXURSH@` — shift right (signed/unsigned)
  - `N_LXMUL@` — multiply
  - `N_LDIV@` / probably `N_LUDIV@` — divide (signed/unsigned)
- **Helper ABI uses STACK args** (not registers like LXMUL@):
  4 words pushed in order LOW(b), HIGH(b), LOW(a), HIGH(a)
  — i.e., divisor first, then dividend. Reading the pushes
  bottom-up, the call sees: `divisor_high, divisor_low,
  dividend_high, dividend_low`.
- **`add sp, 8` cleanup is ELIDED** — BCC relies on the
  function's epilogue `mov sp, bp` (`8b e5`) to restore sp.
  This saves 3 bytes per helper call when applicable.
  Conditions:
  - Function has locals allocated via `sub sp` (so `mov sp, bp`
    is already in the epilogue path).
  - The call is the LAST thing before the epilogue — no
    subsequent stack ops depend on sp being immediately correct.
- The cast `(int)(a/b)` then just keeps AX (the helper's
  DX:AX result low half).

## `s << 16` for long — folded to byte-swap, NO helper call

Fixture `2586-long-lsh-obj`:

```c
long s = 1L;
s = s << 16;
return (int)s;
```

```
55 8b ec 83 ec 04              prologue + 4B local
c7 46 fe 00 00                 s HIGH = 0
c7 46 fc 01 00                 s LOW = 1
8b 46 fc                       ax = s LOW (= 1)
89 46 fe                       [bp-2] = ax       ; s HIGH := old LOW
c7 46 fc 00 00                 [bp-4] = 0        ; s LOW := 0
8b 46 fc                       ax = s LOW (= 0, after shift)
eb 00 8b e5 5d c3              epilogue
```

Findings:
- **`s << 16` on a long is folded at compile time to a WORD SWAP**:
  the LOW word moves to HIGH, the LOW word becomes 0. NO library
  helper call.
- This generalizes: shifts by **exact multiples of 16** on long
  values trade word slots — they can be expressed as moves and
  constants without any actual shift instruction.
  - `<< 16` → low → high, low := 0
  - `<< 32` (but that's all of long) → both halves := 0 (or UB?)
  - `>> 16` (unsigned) → high → low, high := 0
  - `>> 16` (signed) → high → low (sign-extended), high := sign
- For shifts that aren't multiples of 16, BCC falls back to the
  library helper (`N_LXRSH@`, `N_LXURSH@`, `N_LXLSH@`).
- This is a critical peephole — the helper call cost is ~5 bytes
  vs 3-7 bytes for the inline byte-swap.


## Long array initializer — 4 bytes per element, little-endian

Fixture `2588-long-arr-init-obj`:

```c
long table[3] = { 0x12345678L, -1L, 0L };
int main(void) {
  return (int)table[1];
}
```

`_DATA` bytes (12 = 3 × 4):
```
78 56 34 12     ; table[0] = 0x12345678 little-endian
ff ff ff ff     ; table[1] = -1L (two's complement)
00 00 00 00     ; table[2] = 0L
```

Main body:
```
55 8b ec                       prologue
a1 04 00                       mov ax, [_table + 4]  ; LOW word of table[1]
eb 00 5d c3                    epilogue
```

Findings:
- Each long element = 4 bytes packed little-endian.
- `table[K]` for long array folds to byte offset `K × 4`.
- `(int)table[1]` cast: only the low word is loaded — `mov ax,
  [_table + 4]`. The high word at `_table + 6` is unread.
- This matches the per-scalar long layout (`2560`): HIGH word at
  higher address (offset 2 within the long), LOW at offset 0.
- 3-element long array has `sizeof = 12` in `_BSS` / `_DATA`.


## Struct{long} as fn arg + `b.v + 1` — inline `add ax, 1; adc dx, 0`

Fixture `2592-struct-long-arg-obj`:

```c
struct Big { long v; };
long take(struct Big b) {
  return b.v + 1;
}
```

```
55 8b ec                       prologue
8b 56 06                       mov dx, [bp+6]    ; b.v HIGH
8b 46 04                       mov ax, [bp+4]    ; b.v LOW
05 01 00                       add ax, 1         ; LOW + 1
83 d2 00                       adc dx, 0         ; HIGH + carry
eb 00 5d c3                    epilogue
```

Findings:
- 4-byte struct passed by value: caller pushes 4 bytes; callee reads
  LOW at `[bp+4]` and HIGH at `[bp+6]`. Same stack layout as a
  raw `long` arg (LOW lower, HIGH higher).
- **Long + 1 is inlined** as `add ax, imm; adc dx, 0` — NO helper
  call. The 32-bit add is just two 16-bit instructions linked by
  the carry flag.
- `83 d2 00` = `adc reg, imm8` (sign-extended); opcode-ext 010 = adc,
  r/m 010 = dx, then imm8 = 0. So adding a constant 1 to a long is
  `add ax, 1; adc dx, 0` — 3 + 3 = 6 bytes.
- This means **long add/sub by constant is ALWAYS inline**. Only
  long mul/div/shift use helpers. The add+carry pattern handles
  any 32-bit add of `imm16` or `imm32` cleanly.
- For `long + long`, BCC would presumably emit `add ax, [other-LOW];
  adc dx, [other-HIGH]` — likely also inline. To probe.
- The struct-passing convention here is the same as raw-long: the
  struct's fields are LAID OUT identically to the underlying scalar.
  So `struct{long}` and `long` are byte-identical for arg passing.


## `(int)(a + b)` for long+long variable — confirms eager truncation

Fixture `2597-long-add-long-obj`:

```c
long a = 100000L;
long b = 200000L;
return (int)(a + b);
```

```
55 8b ec 83 ec 08              prologue + 8B locals (2 longs)
c7 46 fe 01 00                 a HIGH = 1
c7 46 fc a0 86                 a LOW  = 0x86a0
c7 46 fa 03 00                 b HIGH = 3
c7 46 f8 40 0d                 b LOW  = 0x0d40
8b 46 fc                       mov ax, a.LOW
03 46 f8                       add ax, b.LOW
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Confirms the eager-truncation pattern from `2569`: when the
  enclosing context is `(int)`, BCC emits ONLY the low-word add
  (`add ax, [bp-disp]`), skipping the `adc dx, [bp-disp']` that
  a full long-long add would need.
- This applies to long+long where BOTH operands are variables
  (not just constant + var). Type-directed elision is general.
- Without the cast, the same expression would emit `mov ax, low(a);
  mov dx, high(a); add ax, low(b); adc dx, high(b)`.


## Long return literal — `mov dx, HIGH; mov ax, LOW`

Fixture `2635-fn-return-long-obj`:

```c
long get(void) {
  return 0x12345678L;
}
```

```
55 8b ec                       prologue
ba 34 12                       mov dx, 0x1234   ; HIGH word
b8 78 56                       mov ax, 0x5678   ; LOW word
eb 00 5d c3                    epilogue
```

Findings:
- A long literal returned from a function loads as **two mov-imm16
  instructions in order (HIGH first → DX, then LOW → AX)**.
- Each mov is 3 bytes (opcode + imm16). Total 6 bytes for the
  long value loaded.
- Confirms the return convention: **DX = HIGH word, AX = LOW word**
  for any 32-bit value (long, struct{long}, struct{int,int}).
- Load order matches the data-store order observed in `2521`,
  `2532`, etc.: HIGH first, LOW second.


## Long unary minus `-x` — `neg dx; neg ax; sbb dx, 0` (3 instructions)

Fixture `2673-long-neg-obj`:

```c
long negate(long x) {
  return -x;
}
```

```
55 8b ec                       prologue
8b 56 06                       mov dx, x.HIGH   ; [bp+6]
8b 46 04                       mov ax, x.LOW    ; [bp+4]
f7 da                          neg dx           ; complement HIGH
f7 d8                          neg ax           ; complement LOW (sets borrow)
83 da 00                       sbb dx, 0        ; subtract borrow
eb 00 5d c3                    epilogue
```

Findings:
- Long unary minus = **textbook 3-instruction two's-complement
  negation**:
  1. `neg dx` — flip the HIGH half
  2. `neg ax` — flip the LOW half (sets carry-flag = borrow)
  3. `sbb dx, 0` — propagate the borrow into HIGH
- **NO helper call** — long-neg is inlined like long-add (`2597`)
  and long-add-by-constant (`2592`).
- Long ops summary:

| op                     | inline or helper           |
|------------------------|----------------------------|
| add/sub (+/-, +=/-=)   | inline (add+adc / sub+sbb) |
| neg (`-x`), not (`~x`) | inline (multi-instr)       |
| mul (`*`)              | helper `N_LXMUL@`          |
| div (`/`)              | helper `N_LDIV@`           |
| shift (>> <<)          | helper `N_LXRSH@` / `N_LXURSH@` / `N_LXLSH@` |
| shift by mult of 16    | inline (byte-swap fold, `2586`) |
| compare                | inline (probe needed)      |


## Local `long v = -1L` — two 5-byte word stores

Fixture `2678-local-long-neg-obj`:

```c
long v = -1L;
return (int)v;
```

```
55 8b ec 83 ec 04              prologue + 4B local
c7 46 fe ff ff                 [bp-2] = 0xFFFF    ; v.HIGH = -1
c7 46 fc ff ff                 [bp-4] = 0xFFFF    ; v.LOW = -1
8b 46 fc                       mov ax, [bp-4]     ; (int) cast = low word
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Local long literal store = **two 5-byte `c7 46 disp imm16`**
  instructions. HIGH word stored first to `[bp-2]`, LOW word
  second to `[bp-4]`. Total 10 bytes for the assignment.
- For `-1L`, both halves are `0xFFFF` (two's complement).
- No optimization for "all-1s long" — same instruction count as
  any non-zero long literal.
- For zero-init (`long v = 0L`), BCC would still emit 2 stores
  with `00 00` immediates — no shorter form unless wider init
  forms exist.


## Local `long v = 0x12345678L` — HIGH word first, then LOW

Fixture `2687-local-long-pos-obj`:

```
55 8b ec 83 ec 04              prologue + 4B local
c7 46 fe 34 12                 [bp-2] = 0x1234 (HIGH)
c7 46 fc 78 56                 [bp-4] = 0x5678 (LOW)
8b 46 fc                       mov ax, v.LOW    ; (int) cast
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Confirms the long store pattern: **HIGH word stored first at
  the higher address `[bp-2]`, then LOW at `[bp-4]`**.
- Same shape for positive and negative long literals (`2678`):
  10 bytes for the assignment (2 × 5-byte `mov word [bp+disp], imm16`).


## `(int)long_var` for a long param — single `mov ax, [bp+4]` (load LOW only)

Fixture `2732-int-from-long-obj`:

```c
int narrow(long v) {
  return (int)v;
}
```

```
55 8b ec                       prologue
8b 46 04                       mov ax, [bp+4]    ; LOW word only
eb 00 5d c3                    epilogue
```

Findings:
- `(int)long_var` truncates by loading ONLY the low word at the
  long's base address. The HIGH word at `[bp+6]` is unread.
- Single 3-byte `mov ax, [mem]` for the entire cast + return.
- Same shape as `(int)long_local` (`2521`) and `(int)long_global`
  (`2560`).


## `arr[K]` for `long arr[]` const subscript — high+low load pair

Fixture `2782-long-arr-elem-obj`:

```c
long arr[3] = { 100L, 200L, 300L };
long pick(void) { return arr[1]; }
```

```
8b 16 06 00                    mov dx, [_arr + 6]   ; HIGH word of arr[1]
a1 04 00                       mov ax, [_arr + 4]   ; LOW word of arr[1]
```

`_DATA` layout (12 bytes):
- arr[0]: `64 00 00 00` (= 100L)
- arr[1]: `c8 00 00 00` (= 200L)
- arr[2]: `2c 01 00 00` (= 300L)

Findings:
- `long arr[K]` const subscript folds to two loads at byte offsets
  `K × 4` and `K × 4 + 2`.
- **Load order: HIGH word first** (`mov dx, [+6]`), then LOW word
  (`mov ax, [+4]`). Matches the convention for direct long loads.
- Total 7 bytes for the read (4B for dx + 3B for ax).
- Compare to int arr[K] which is 1 word load (3B).


## `int x = (int)L;` for global long — single LOW-word load

Fixture `2788-int-from-long-asgn-obj`:

```c
long L = 0x12345678L;
int main(void) {
  int x = (int)L;
  return x;
}
```

`_DATA` (4 bytes for L): `78 56 34 12` (= 0x12345678 little-endian)

```
4c 4c                          dec sp twice (2B for x)
a1 00 00                       mov ax, [_L]    ; LOW word only (= 0x5678)
89 46 fe                       x = ax
8b 46 fe                       reload x for return
```

Findings:
- `(int)long_global` = single `mov ax, [_L]` (3B with FIXUPP). The
  high word at `[_L + 2]` is unread. Same shape as `(int)long_local`
  (`2521`), `(int)long_param` (`2732`), and `(int)long_global` in
  `2560`.
- `_DATA` long bytes are little-endian: `0x12345678` → `78 56 34 12`.
- The conversion is byte truncation at the load site, no
  bit-manipulation instructions.


## Long `>> 2` (signed) — `N_LXRSH@` helper with CL count

Fixture `2789-local-long-shr-obj`:

```c
long shrink(long v) {
  return v >> 2;
}
```

```
8b 56 06                       mov dx, v.high
8b 46 04                       mov ax, v.low
b1 02                          mov cl, 2 (count)
e8 00 00                       call N_LXRSH@
```

Findings:
- Long signed right-shift uses **`N_LXRSH@` helper**, never inline.
- DX:AX = long operand, CL = shift count (set before call).
- Total 11 bytes for the shift (8B setup + 3B call FIXUPP).
- For `>> 16` (multiple of 16), BCC would use byte-swap fold (see
  long-codegen earlier) instead of the helper.

## Global ulong load — HIGH-then-LOW pair (standard long return)

Fixture `2790-global-ulong-obj`:

`_DATA` for `_counter` (4 bytes): `be ba fe ca` (= 0xCAFEBABE LE)

```
8b 16 02 00                    mov dx, [_counter + 2] (HIGH)
a1 00 00                       mov ax, [_counter]     (LOW)
```

Findings:
- Returning a long-typed global = standard HIGH-then-LOW load pair.
- 7 bytes total (4B for dx + 3B for ax).
- Signed-vs-unsigned long load is IDENTICAL bytes — the
  type interpretation matters only for ops that read it.


## Signed `long >> 16` — byte-swap fold (4 bytes total!)

Fixture `2795-long-shr-16-obj`:

```c
long top(long v) {
  return v >> 16;
}
```

```
8b 46 06                       mov ax, v.HIGH   ; new low = old high
99                             cwd              ; sign-extend → new high
```

Findings:
- `v >> 16` for signed long compiles to **`mov ax, [bp+6]; cwd`**
  — just 4 bytes total!
- The semantics: shifting a 32-bit value right by 16 makes the new
  LOW word = the old HIGH word; the new HIGH word = the sign bit
  of the old HIGH word (replicated by `cwd`).
- For unsigned, BCC would emit `mov ax, [bp+6]; xor dx, dx` (5B,
  zero-fill instead of sign-fill).
- Compare to `>> 2` (`2789`) which uses the 11-byte helper call.
- **Lesson**: source-form choice matters — write `>> 16` for the
  fast byte-swap fold; other shift amounts pay the helper cost.


## Local `long arr[3]` with var-index access — BX-based address calc

Fixture `2798-local-long-arr-obj`:

```c
long arr[3];
arr[0] = 100L; arr[1] = 200L; arr[2] = 300L;
return arr[i];
```

```
83 ec 0c                       sub sp, 12 (3 longs)
                               ; per-element init (HIGH first, then LOW):
c7 46 f6 00 00                 arr[0].HIGH = 0
c7 46 f4 64 00                 arr[0].LOW = 100
...
                               ; var-index access:
8b 5e 04                       mov bx, i
d1 e3 d1 e3                    shl bx, 1 × 2     ; i * 4 (sizeof(long))
8d 46 f4                       lea ax, [bp-12]   ; &arr[0]
03 d8                          add bx, ax        ; bx = &arr[i]
8b 57 02                       mov dx, [bx + 2]  ; HIGH word
8b 07                          mov ax, [bx]      ; LOW word
```

Findings:
- Long-array variable index uses **BX as the address register**,
  computed via `mov + shl×2 + lea + add` (10 bytes).
- Init order per element: **HIGH first, then LOW** (offset+2, then
  offset+0). Same as global long arrays (`2782`).
- Once BX = &arr[i], the load is the standard 5-byte HIGH+LOW pair.


## `unsigned long a * b` — `N_LXMUL@` with fast-call regs (CX:BX, DX:AX)

Fixture `2799-ulong-mul-obj`:

```c
unsigned long product(unsigned long a, unsigned long b) {
  return a * b;
}
```

```
8b 4e 06                       mov cx, a.HIGH (= [bp+6])
8b 5e 04                       mov bx, a.LOW  (= [bp+4])
8b 56 0a                       mov dx, b.HIGH (= [bp+10])
8b 46 08                       mov ax, b.LOW  (= [bp+8])
e8 00 00                       call N_LXMUL@   (FIXUPP)
```

Findings:
- Long multiplication uses **`N_LXMUL@` helper with FAST-CALL**:
  - **CX:BX** = first operand (high:low)
  - **DX:AX** = second operand (high:low)
  - Return: DX:AX
- Args loaded directly into registers — no stack push.
- Same helper for signed and unsigned (since multiply low 32 bits
  is the same regardless of signedness).
- Compare to long divide (`N_LDIV@`) which uses STACK args
  (different convention per helper).


## Unsigned `long >> 16` — `mov ax, HIGH; xor dx, dx` (5B, 1B BIGGER than signed!)

Fixture `2801-ulong-shr-16-obj`:

```c
unsigned long top(unsigned long v) {
  return v >> 16;
}
```

```
8b 46 06                       mov ax, v.HIGH
33 d2                          xor dx, dx     ; ZERO-fill (unsigned)
```

Findings:
- Unsigned `>> 16` uses `mov + xor dx, dx` (5 bytes).
- Signed `>> 16` (`2795`) uses `mov + cwd` (4 bytes).
- **Surprising**: unsigned is **1 byte LARGER** for this operation!
  Usually unsigned saves bytes (no cbw/cwd), but the byte-swap
  fold for `>> 16` specifically needs to zero-fill the high word —
  cwd (1B) replicates sign cheaper than xor dx, dx (2B) zeroes.

## `if (g)` for global long — `or` peephole combines halves

Fixture `2806-if-long-test-obj`:

```c
long g_count;
if (g_count) return 1;
return 0;
```

```
a1 00 00                       mov ax, [_g_count]      (LOW)
0b 06 02 00                    or ax, [_g_count + 2]   (HIGH)
74 05                          je → ZERO
```

Findings:
- **Long zero-test peephole**: load LOW into AX, then **or with HIGH
  word in memory** (`0b 06 disp16`, 4B). If the OR result is zero,
  both halves were zero → long is zero.
- Total test cost: 3B (load) + 4B (or-with-mem) + 2B (je) = **9
  bytes** for the long zero-test.
- Much cheaper than testing each half separately (would be 14+B).
- Sign-irrelevant: same shape for signed and unsigned longs.


## Local `long v = 0x12345678L;` — HIGH-first stores

Fixture `2828-local-long-init-obj`:

```c
long v = 0x12345678L;
return v;
```

```
83 ec 04                       sub sp, 4
c7 46 fe 34 12                 [bp-2] = 0x1234 (HIGH word)
c7 46 fc 78 56                 [bp-4] = 0x5678 (LOW word)
8b 56 fe                       mov dx, v.HIGH
8b 46 fc                       mov ax, v.LOW
```

Findings:
- Local long init writes **HIGH first** (`[bp-2]`), then LOW
  (`[bp-4]`). Storage layout: LOW at lower address (`[bp-4]`),
  HIGH at higher address (`[bp-2]`).
- Read order also HIGH-then-LOW (`mov dx, HIGH; mov ax, LOW`),
  matching long-return ABI.
- Same shape as global long return (`2790`).

## `ulong / 10UL` — `N_LUDIV@` helper with STACK args

Fixture `2829-ulong-div-10-obj`:

```c
unsigned long div10(unsigned long v) {
  return v / 10UL;
}
```

```
33 c0                          xor ax, ax       (HIGH of 10UL = 0)
ba 0a 00                       mov dx, 10       (LOW of 10UL)
50                             push ax          (push HIGH first)
52                             push dx          (push LOW)
ff 76 06                       push v.HIGH
ff 76 04                       push v.LOW
e8 00 00                       call N_LUDIV@    (FIXUPP)
                               ; NO visible cleanup — helper self-cleans
```

Findings:
- Unsigned long divide uses **`N_LUDIV@` helper with STACK args**
  (different from `N_LXMUL@` fast-call regs).
- Stack layout from top: `[sp] = numerator.LOW, [sp+2] = numerator.HIGH,
  [sp+4] = divisor.LOW, [sp+6] = divisor.HIGH`.
- Helper does its own arg cleanup (no `add sp, N` at caller).
- Return: DX:AX = quotient.
- For signed long divide: would use `N_LDIV@` (different helper).
- The constant 10UL is materialized via `xor ax,ax; mov dx, 10`
  (efficient: 5 bytes for the 4-byte long).


## Signed `long a < b` — INLINE 3-step lexicographic compare (no helper!)

Fixture `2864-long-cmp-lt-obj`:

```c
int less_long(long a, long b) {
  if (a < b) return 1;
  return 0;
}
```

```
8b 46 06                       mov ax, a.HIGH ([bp+6])
8b 56 04                       mov dx, a.LOW  ([bp+4])
3b 46 0a                       cmp ax, b.HIGH ([bp+10])
7f 0c                          jg  +12 → FALSE  (a.HIGH > b.HIGH, definitive)
7c 05                          jl  +5  → TRUE   (a.HIGH < b.HIGH, definitive)
3b 56 08                       cmp dx, b.LOW ([bp+8])
73 05                          jae +5 → FALSE  (UNSIGNED: dx >= b.LOW → a >= b)
                               ; TRUE:
b8 01 00 eb 04                 return 1
                               ; FALSE:
33 c0                          return 0
```

Findings:
- **Long signed compare is INLINE, NOT a helper call.**
- 3-step lexicographic compare:
  1. **Compare HIGH words SIGNED** (for the sign bit / overall sign)
  2. If HIGH equal, **compare LOW words UNSIGNED** (low halves are
     bit patterns, not signed)
  3. Combine results
- ~18-20 bytes for the long compare vs 6 bytes for int compare.
- Why no helper? Compare is just cmp+branch — no helper needed.
  Long ops that need real computation (mul, div, shift) use helpers.
- **Subtle**: the LOW compare uses UNSIGNED form (`jae`/`jb`) even
  though we're comparing signed longs. The signed-ness is encoded
  in the HIGH word; the LOW word is a magnitude.


## Unsigned long `a < b` — UNSIGNED for BOTH halves

Fixture `2867-ulong-cmp-lt-obj`:

```c
int u_less(unsigned long a, unsigned long b) {
  if (a < b) return 1;
  return 0;
}
```

```
3b 46 0a                       cmp ax, b.HIGH
77 0c                          ja  → FALSE   (UNSIGNED!)
72 05                          jb  → TRUE    (UNSIGNED!)
3b 56 08                       cmp dx, b.LOW
73 05                          jae → FALSE   (UNSIGNED)
```

Findings:
- Unsigned long compare uses **unsigned jumps for both HIGH and LOW**.
- Compare to signed long (`2864`) which uses **signed for HIGH,
  unsigned for LOW**.
- The sign-handling difference is in the HIGH-word compare only.
- Updated long-compare rule:

| compare    | HIGH jump  | LOW jump   |
|------------|------------|------------|
| signed     | jg/jl etc  | jae/jb etc (always unsigned for low) |
| unsigned   | ja/jb etc  | jae/jb etc |

## Long `a == b` — two sequential cmp-jne, both target FALSE

Fixture `2868-long-cmp-eq-obj`:

```
cmp ax, b.HIGH
75 0a                          jne → FALSE
cmp dx, b.LOW
75 05                          jne → FALSE
                               ; TRUE: ...
```

Findings:
- Long `==` = TWO cmp-jne, both branch to FALSE.
- If EITHER half differs, immediately FALSE.
- Fall-through to TRUE only if both halves match.
- ~18 bytes total for the compare.

## Long `a != b` — early-exit on true (dual of `==`)

Fixture `2869-long-cmp-ne-obj`:

```
cmp ax, b.HIGH
75 05                          jne → TRUE     (HIGH differs, defin. !=)
cmp dx, b.LOW
74 05                          je  → FALSE    (HIGH=, LOW= → they ARE equal)
                               ; TRUE: ...
```

Findings:
- Long `!=` = HIGH differs → immediate TRUE. Else LOW equal → FALSE.
  Else (LOW differs) → TRUE via fall-through.
- Optimized branch directions: jne for "early exit true", je for "early exit false".
- ~18 bytes total — same size as `==`, different branch directions.

## Long subtract `a - b` — INLINE `sub + sbb` (4-byte op chain)

Fixture `2870-long-sub-obj`:

```c
long diff(long a, long b) {
  return a - b;
}
```

```
8b 56 06                       mov dx, a.HIGH
8b 46 04                       mov ax, a.LOW
2b 46 08                       sub ax, b.LOW
1b 56 0a                       sbb dx, b.HIGH    (subtract WITH BORROW)
```

Findings:
- Long subtract is INLINE: `sub` for LOW, `sbb` for HIGH.
- `sbb` propagates the borrow flag from `sub`.
- Same pattern as long ADD (uses `add + adc`).
- No helper needed for add/sub since x86 has carry/borrow chain.
- Total 12 bytes for the expression body.

## Unsigned long add `a + b` — INLINE `add + adc` (signed/unsigned identical)

Fixture `2872-ulong-add-obj`:

```c
unsigned long add(unsigned long a, unsigned long b) {
  return a + b;
}
```

```
8b 56 06                       mov dx, a.HIGH
8b 46 04                       mov ax, a.LOW
03 46 08                       add ax, b.LOW
13 56 0a                       adc dx, b.HIGH    (add WITH CARRY)
```

Findings:
- Long add: `add` for LOW, `adc` for HIGH (carry chain).
- **Signed and unsigned long add are byte-identical** — bit-level
  operation is the same.
- 12 bytes total for the expression.


## Long bitwise INLINE family: OR/AND (per-word, no carry chain)

Fixtures `2873-long-or-obj`, `2874-long-and-obj`:

```c
long lor(long a, long b)  { return a | b; }
long land(long a, long b) { return a & b; }
```

```
                               ; long OR:
0b 46 08                       or ax, b.LOW
0b 56 0a                       or dx, b.HIGH

                               ; long AND:
23 46 08                       and ax, b.LOW
23 56 0a                       and dx, b.HIGH
```

Findings:
- Long bitwise OR/AND are **INLINE per-word ops** (no helper, no
  carry chain — bitwise ops don't have inter-word dependencies).
- 12 bytes total for the expression body.
- Same shape as long add/sub but using OR/AND opcodes (`0b /r`,
  `23 /r`).

## Long `<< 16` — BYTE-SWAP FOLD for left shift (5 bytes)

Fixture `2875-long-shl-16-obj`:

```c
long swap(long v) {
  return v << 16;
}
```

```
8b 56 04                       mov dx, v.LOW   ; new HIGH = old LOW
33 c0                          xor ax, ax      ; new LOW = 0
```

Findings:
- Long `<< 16` = **MIRROR of `>> 16`**: 5-byte byte-swap fold.
  - New HIGH ← old LOW (the high word now holds what was in low)
  - New LOW ← 0 (zero-filled from the right)
- No helper call, no actual shifting — just word rearrangement.
- Compare to unsigned `>> 16`: same 5-byte shape, opposite direction.

## Long `a | 0x100L` — per-word OR with imm (no `|0` peephole)

Fixture `2876-long-or-imm-obj`:

```c
long mask(long a) {
  return a | 0x100L;
}
```

```
0d 00 01                       or ax, 0x0100  (LOW word, AX-acc form 3B)
81 ca 00 00                    or dx, 0       (HIGH word, ModR/M 4B)
```

Findings:
- Long OR-with-constant emits **per-word OR** for both halves.
- HIGH word OR with 0 is emitted EXPLICITLY — BCC does NOT fold
  `| 0` to identity for long constants (same as `& 0` (`2737`)).
- LOW word uses 3B AX-acc form (`0d imm16`), HIGH uses 4B
  ModR/M form (`81 /1 imm16`). 7 bytes total.

## Long `-a` (negate) — `neg dx; neg ax; sbb dx, 0` (7 bytes)

Fixture `2877-long-neg-obj`:

```c
long lneg(long a) {
  return -a;
}
```

```
f7 da                          neg dx  (HIGH)
f7 d8                          neg ax  (LOW; sets CF if LOW != 0)
83 da 00                       sbb dx, 0  (propagate borrow)
```

Findings:
- Long negate is **INLINE 7-byte sequence**:
  1. `neg dx` — negate HIGH word (initial guess)
  2. `neg ax` — negate LOW word; CF set if LOW != 0 (there was borrow)
  3. `sbb dx, 0` — subtract the borrow from HIGH
- This correctly computes `-N = ~N + 1` for multi-word values.
- For N=0: no borrow, dx and ax both stay 0. For N!=0: borrow
  propagates correctly.

## Long `a << 1` — INLINE `shl ax, 1; rcl dx, 1` (4 bytes!)

Fixture `2878-long-shl-1-obj`:

```c
long lshl(long a) {
  return a << 1;
}
```

```
d1 e0                          shl ax, 1   (LOW, top bit → CF)
d1 d2                          rcl dx, 1   (HIGH rotate left through carry)
```

Findings:
- Long `<< 1` is **INLINE, 4 bytes!** Uses `shl + rcl` (rotate
  through carry) to chain the carry from LOW to HIGH.
- Compare to `>> 2` (`2789`) which used `N_LXRSH@` helper.
- Compare to `<< 16` (`2875`) which uses byte-swap fold (5B).
- Conjecture: `<< 1` is special-cased to inline; multi-bit
  left-shifts (`<< 2`, `<< 3`, etc.) likely use the helper.
- 8086 has `rcl r16, 1` opcode `d1 d2` (rotate left 1 through CF).


## Long `<< 2` — `N_LXLSH@` helper (only `<< 1` is inlined)

Fixture `2879-long-shl-2-obj`:

```c
long lshl2(long a) {
  return a << 2;
}
```

```
8b 56 06                       mov dx, a.HIGH
8b 46 04                       mov ax, a.LOW
b1 02                          mov cl, 2
e8 00 00                       call N_LXLSH@  (long left-shift helper)
```

Findings:
- Long `<< 2` uses **`N_LXLSH@` helper** with CL=count, DX:AX=value.
- Confirms long shift policy:

| shift amount | strategy        | bytes |
|--------------|-----------------|-------|
| `<< 1`       | inline `shl+rcl`| 4B    |
| `<< 16`      | byte-swap fold  | 5B    |
| `<< 32`      | zero both halves (probable) | 4B |
| `<< 2+`      | `N_LXLSH@` helper | 8B  |

- Only `<< 1` and shifts-by-multiples-of-16 are special-cased.
  All others pay the helper call.

## Unsigned long `<< 1` — byte-identical to signed `<< 1`

Fixture `2880-ulong-shl-1-obj`:

```
d1 e0                          shl ax, 1
d1 d2                          rcl dx, 1
```

Same body as signed `<< 1` (`2878`). Left shift is signedness-agnostic
(top bit is just bit 31 of the 32-bit value).


## Long `>> 1` — INLINE `sar dx, 1; rcr ax, 1` (4 bytes, mirror of `<< 1`)

Fixture `2885-long-shr-1-obj`:

```c
long lshr1(long a) {
  return a >> 1;
}
```

```
d1 fa                          sar dx, 1   (HIGH: signed shift right)
d1 d8                          rcr ax, 1   (LOW: rotate right through carry)
```

Findings:
- Signed long `>> 1` is **INLINE 4 bytes** — mirror of `<< 1`.
- Order: HIGH first (sar), then LOW (rcr) — carry flows from HIGH
  to LOW (opposite direction from left-shift).
- This **contradicts** my earlier finding for `>> 2` (`2789`) which
  used the helper — confirming the policy: **`>> 1` is inlined,
  `>> 2+` uses `N_LXRSH@`** (mirror of left-shift policy).

## Unsigned long `>> 1` — `shr dx, 1; rcr ax, 1` (4B, different HIGH opcode)

Fixture `2886-ulong-shr-1-obj`:

```
d1 ea                          shr dx, 1   (HIGH: unsigned)
d1 d8                          rcr ax, 1   (LOW: rotate)
```

Findings:
- Unsigned long `>> 1` differs ONLY in the HIGH-word opcode:
  - Signed: `sar dx, 1` (`d1 fa`, op-ext 7)
  - Unsigned: `shr dx, 1` (`d1 ea`, op-ext 5)
- LOW word always uses `rcr ax, 1` (rotate-through-carry has no
  sign distinction).
- Same 4-byte cost for both.


## Local `long b = a;` — 4-mov sequence (12 bytes)

Fixture `2912-long-copy-obj`:

```c
long copy(long a) {
  long b = a;
  return b;
}
```

```
83 ec 04                       sub sp, 4 (local b)
8b 46 06                       mov ax, a.HIGH
8b 56 04                       mov dx, a.LOW
89 46 fe                       b.HIGH = ax
89 56 fc                       b.LOW = dx
8b 56 fe                       mov dx, b.HIGH (return)
8b 46 fc                       mov ax, b.LOW
```

Findings:
- Long-to-long copy = **4 mov instructions** (12 bytes for the copy
  portion).
- AX carries HIGH, DX carries LOW during the copy. Return ABI is
  DX:AX = HIGH:LOW.
- Each word goes through its own register; no helper needed.


## Signed long compare `v < 0L` — HIGH signed cmp + LOW unsigned cmp

Fixture `3026-signed-long-lt-obj`:

```c
if (v < 0L) return 1;
```

```
83 7e 06 00                    cmp word [bp+6], 0   (HIGH)
7f 0d                          jg → FALSE   (signed: HIGH > 0 means v > 0)
7c 06                          jl → TRUE    (signed: HIGH < 0 means v < 0)
                               ; HIGH == 0, check LOW:
83 7e 04 00                    cmp word [bp+4], 0   (LOW)
73 05                          jae → FALSE  (UNSIGNED: LOW >= 0 means v >= 0)
                               ; TRUE:
```

Findings:
- **HIGH word**: signed compare (`jg`/`jl`).
- **LOW word**: UNSIGNED compare (`jae`/`jb`) — the low bits are
  treated as magnitude when high is equal.
- Confirms the 3-step lexicographic long comparison from
  `long-codegen.md`.


## `long > 100L` const compare — HIGH=0, LOW=imm8 sign-ext

Fixture `3050-long-cmp-100-obj`:

```c
if (v > 100L) return 1;
```

```
83 7e 06 00                    cmp word [bp+6], 0       (HIGH, signed)
7c 0d                          jl → FALSE   (HIGH < 0)
7f 06                          jg → TRUE    (HIGH > 0)
                               ; HIGH == 0:
83 7e 04 64                    cmp word [bp+4], 100     (LOW, unsigned imm8 sign-ext)
76 05                          jbe → FALSE  (UNSIGNED: LOW <= 100)
                               ; TRUE
```

Findings:
- Const long `100L` = `0x00000064`: HIGH = 0, LOW = 100.
- HIGH compared signed (`jl`/`jg`); LOW compared **unsigned**
  (`jbe`/`ja`) since low half is positional.
- Both halves use `83 /7 imm8` (4B each, fits signed imm8).
- 3-step lexicographic, same as `2820` (`l > 0L`) but with non-zero LOW.

## AX/DX register assignment for long ops — flexible (LOW or HIGH)

Fixture `3051-static-long-init-obj`:

```c
static long counter = 0L;
counter = counter + 1L;
```

During `+= 1L`:
- AX = HIGH (loaded from `[mem + 2]`)
- DX = LOW  (loaded from `[mem + 0]`)
- `add dx, 1; adc ax, 0` (LOW first, then HIGH with carry)

For function RETURN (long), convention is **DX:AX with DX=HIGH,
AX=LOW**. Reload from mem before returning.

Findings:
- The AX/DX assignment during long arithmetic is FLEXIBLE — BCC
  picks based on the operation's needs.
- The long-return convention is fixed: `DX:AX` where DX=HIGH, AX=LOW.
- If arithmetic results land in the "wrong" reg, BCC reloads from
  memory to set up the right return convention.


## Unsigned long compare — BOTH halves use UNSIGNED jumps

Fixture `3058-ulong-cmp-const-obj`:

```c
unsigned long v;
if (v > 1000UL) return 1;
```

```
83 7e 06 00                    cmp word [bp+6], 0      (HIGH)
72 0e                          jb → FALSE   (UNSIGNED: HIGH < 0 impossible)
77 07                          ja → TRUE    (UNSIGNED: HIGH > 0)
                               ; HIGH == 0:
81 7e 04 e8 03                 cmp word [bp+4], 1000   (LOW, imm16)
76 05                          jbe → FALSE  (UNSIGNED: LOW <= 1000)
                               ; TRUE
```

Findings:
- **Unsigned long**: BOTH HIGH and LOW use UNSIGNED jumps (`jb`/`ja`/`jbe`).
- **Signed long** (`3026`): HIGH signed (`jl`/`jg`), LOW unsigned.
- Difference is in the HIGH word's jump kind.
- Const 1000 > signed imm8 max (127) so LOW cmp uses `81 imm16` form (5B).


## Long return convention CONFIRMED — DX:AX with DX=HIGH, AX=LOW

Fixture `3066-long-return-obj`:

```c
return 0x12345678L;
```

```
ba 34 12                       mov dx, 0x1234   (HIGH word)
b8 78 56                       mov ax, 0x5678   (LOW word)
```

Findings:
- Long return value is in **DX:AX** with DX holding HIGH and AX
  holding LOW.
- 6 bytes for a long literal return (2 `mov reg, imm16`).
- Caller reads result as `(DX << 16) | AX`.

## `long + 1L` — `add ax, 1; adc dx, 0` (5-step long add)

Fixture `3067-long-plus-1L-obj`:

```c
long inc(long v) { return v + 1L; }
```

```
8b 56 06                       mov dx, [bp+6]   (load HIGH)
8b 46 04                       mov ax, [bp+4]   (load LOW)
05 01 00                       add ax, 1        (LOW + 1, AX-acc imm16)
83 d2 00                       adc dx, 0        (HIGH + carry)
```

Findings:
- 32-bit add of long + small const:
  1. Load DX:AX from operands
  2. `add ax, imm16` for LOW
  3. `adc dx, 0` for HIGH (absorbs carry from LOW)
- 6 bytes for the compute (load excluded).
- Const 1L = HIGH=0, LOW=1 split. Only LOW gets the actual add value.
- For larger constants where HIGH is non-zero, both halves would
  use add/adc with the respective constants.


## Local `long += 1L` — `add word [mem]` + `adc word [mem]` (mem-imm)

Fixture `3094-local-long-plus-eq-obj`:

```c
long bump(long n) {
  n += 1L;
  return n;
}
```

```
83 46 04 01                    add word [bp+4], 1   (LOW + 1, imm8 sign-ext)
83 56 06 00                    adc word [bp+6], 0   (HIGH + carry)
8b 56 06                       mov dx, [bp+6]       (load HIGH for return)
8b 46 04                       mov ax, [bp+4]       (load LOW for return)
```

Findings:
- Local long `+= K` uses **mem-imm add then adc** (4B each via
  `83 /0` and `83 /2` with imm8 sign-ext).
- 8 bytes for the compute (add + adc).
- 6 bytes to load result into DX:AX return convention.
- Note ModR/M ops: `83 /0` = add, `83 /2` = adc. Both use imm8
  sign-ext when constant fits.


## Long negation `-v` — `neg dx; neg ax; sbb dx, 0`

Fixture `3158-long-neg-obj`:

```c
long neg(long v) { return -v; }
```

```
8b 56 06                       mov dx, HIGH
8b 46 04                       mov ax, LOW
f7 da                          neg dx
f7 d8                          neg ax
83 da 00                       sbb dx, 0
```

Findings:
- 8086 32-bit negation algorithm:
  1. `neg dx` (high half, sets CF if was non-zero)
  2. `neg ax` (low half, sets CF if was non-zero)
  3. `sbb dx, 0` (adjusts DX by subtracting CF from LOW negation)
- The `sbb` corrects for the borrow that propagates from LOW.
- 13 bytes total (with the DX:AX load).
- ModR/M `da` = mod 11, op-ext 011 (neg /3), r/m 010 (DX).

## `(unsigned long)unsigned_int` — `xor dx, dx` zero-extend

Fixture `3155-uint-to-ulong-obj`:

```c
unsigned long widen(unsigned int x) { return (unsigned long)x; }
```

```
8b 46 04                       mov ax, x
33 d2                          xor dx, dx     (zero-extend)
```

Findings:
- Unsigned widening = `xor dx, dx` (2B).
- Signed widening (`3154`) = `cwd` (1B sign-extend).
- **Unsigned is 1 byte LONGER** than signed for int→long.


## Local `long v <<= 1` — `shl dx, 1; rcl ax, 1` (carry chain)

Fixture `3162-local-long-shl-1-obj`:

```c
long v;
v <<= 1;
```

```
8b 46 06                       mov ax, HIGH
8b 56 04                       mov dx, LOW
d1 e2                          shl dx, 1    (LOW shift, sets CF)
d1 d0                          rcl ax, 1    (HIGH rotate-with-carry from LOW)
89 46 06                       store HIGH
89 56 04                       store LOW
8b 56 06                       reload HIGH for return DX:AX
8b 46 04                       reload LOW
```

Findings:
- Long `<<= 1` = `shl LOW; rcl HIGH` (carry-chain shift).
- `rcl` pulls the bit shifted OUT of LOW INTO HIGH's low bit.
- During compute, AX=HIGH, DX=LOW (reversed from return convention).
- Store-back + reload restores return convention DX:AX.
- 22 bytes total — could be shorter but BCC stores+reloads.

## `long a + b` — load DX:AX = a, then mem-source add+adc for b

Fixture `3164-long-add-obj`:

```c
return a + b;
```

```
8b 56 06                       mov dx, a HIGH
8b 46 04                       mov ax, a LOW
03 46 08                       add ax, [bp+8]   (b LOW, mem-source)
13 56 0a                       adc dx, [bp+10]  (b HIGH, with carry)
```

Findings:
- 12 bytes total: 6B load + 6B add/adc.
- Second long operand added DIRECTLY from memory (no separate load).
- ModR/M ops: `03 /r` = add, `13 /r` = adc.
- Carry chain: add for LOW, adc for HIGH.


## `long a - b` — `sub + sbb` mem-source (mirror of add)

Fixture `3167-long-sub-obj`:

```
8b 56 06                       mov dx, a HIGH
8b 46 04                       mov ax, a LOW
2b 46 08                       sub ax, [bp+8]   (LOW - b LOW)
1b 56 0a                       sbb dx, [bp+10]  (HIGH - b HIGH - borrow)
```

Findings:
- 12 bytes (6B load + 6B sub/sbb).
- `2b /r` = sub, `1b /r` = sbb (with borrow).
- Mirror of long add (`3164`).

## `unsigned long a * b` — `N_LXMUL@` helper, fast-call CX:BX × DX:AX

Fixture `3168-ulong-mul-obj`:

```c
return a * b;
```

```
8b 4e 06                       mov cx, a HIGH    (→ CX)
8b 5e 04                       mov bx, a LOW     (→ BX)
8b 56 0a                       mov dx, b HIGH    (→ DX)
8b 46 08                       mov ax, b LOW     (→ AX)
e8 00 00                       call N_LXMUL@
```

Findings:
- Long multiply uses **`N_LXMUL@` helper**.
- Fast-call: `CX:BX` × `DX:AX` → `DX:AX` (result).
- 15 bytes for the setup + call.

## `long << N` for N ≥ 2 — `N_LXLSH@` helper

Fixture `3169-long-shl-4-obj`:

```c
return v << 4;
```

```
8b 56 06                       mov dx, HIGH
8b 46 04                       mov ax, LOW
b1 04                          mov cl, 4
e8 00 00                       call N_LXLSH@
```

Findings:
- Long shift by N ≥ 2 uses **`N_LXLSH@` helper**.
- DX:AX = value, CL = count.
- 11 bytes setup + call.
- **Threshold**: `<< 1` inline (per `3162`), `<< N≥2` helper.

## `long * 2L` — STRENGTH-REDUCED to `shl + rcl` (NOT helper!)

Fixture `3170-long-mul-2-obj`:

```c
return v * 2L;
```

```
8b 56 06                       mov dx, HIGH
8b 46 04                       mov ax, LOW
d1 e0                          shl ax, 1
d1 d2                          rcl dx, 1
```

Findings:
- `long * 2L` IS strength-reduced to `shl LOW; rcl HIGH` (10B).
- **NOT** N_LXMUL@ helper.
- Multiplication is always safe to reduce to shift (no rounding).
- Saves vs N_LXMUL@ which would be 15B+ with helper-call overhead.


## `long a / b` — `N_LDIV@` helper (stack args, self-cleanup)

Fixture `3173-long-div-obj`:

```c
return a / b;
```

```
ff 76 0a                       push b HIGH
ff 76 08                       push b LOW
ff 76 06                       push a HIGH
ff 76 04                       push a LOW
e8 00 00                       call N_LDIV@
```

Findings:
- Signed long div uses **`N_LDIV@` helper** with **stack args**.
- All 4 words pushed in order: b HIGH, b LOW, a HIGH, a LOW.
- **NO `add sp` follows** — N_LDIV@ does self-cleanup (RET 8 internally).
- 11 bytes setup + call.

## `unsigned long a / b` — `N_LUDIV@` helper (same calling convention)

Fixture `3174-ulong-div-obj`:

Same 4-push pattern, just different helper name (`N_LUDIV@`).

## `long * 4L` — `<< 2` → `N_LXLSH@` helper (strength-reduced to shift)

Fixture `3175-long-mul-4L-obj`:

```c
return v * 4L;
```

```
8b 56 06                       mov dx, HIGH
8b 46 04                       mov ax, LOW
b1 02                          mov cl, 2     (= log2(4))
e8 00 00                       call N_LXLSH@
```

Findings:
- `long * 4L` = strength-reduce to `<< 2` → N_LXLSH@ helper.
- 11 bytes setup + call vs ~15+ for N_LXMUL@.
- General rule: long `* (1<<N)` for N ≥ 2 strength-reduces to shift+helper.
- `* 2L` (N=1) uses inline `shl + rcl` (`3170`).

## `long / 2L` — NOT strength-reduced, uses `N_LDIV@`

Fixture `3176-long-div-2L-obj`:

```
33 c0                          xor ax, ax    (divisor HIGH = 0)
ba 02 00                       mov dx, 2     (divisor LOW = 2)
50 52                          push ax; push dx   (b HIGH, b LOW)
ff 76 06                       push a HIGH
ff 76 04                       push a LOW
e8 00 00                       call N_LDIV@
```

Findings:
- Signed long `/ 2L` is **NOT strength-reduced**.
- Same reason as `int / 2` (`3088`): signed shift rounds toward -∞,
  C div rounds toward 0.
- Constant divisor (2L) loaded into DX:AX and pushed.


## Long shift right helpers — signed `N_LXRSH@` vs unsigned `N_LXURSH@`

Fixtures `3179-long-sar-4-obj`, `3180-ulong-shr-4-obj`:

```c
long v >> 4;          /* signed:   N_LXRSH@  */
unsigned long v >> 4; /* unsigned: N_LXURSH@ */
```

Both use same fast-call convention: DX:AX value, CL count.

Findings:
- **Signed long `>> N`**: `N_LXRSH@` (8 chars).
- **Unsigned long `>> N`**: `N_LXURSH@` (9 chars).
- Different helpers because signed uses sar semantics (sign-fill),
  unsigned uses shr semantics (zero-fill).

## Long mod `a % b` — `N_LMOD@` helper

Fixture `3181-long-mod-obj`:

```c
return a % b;
```

Same 4-push pattern as `N_LDIV@`, with helper name `N_LMOD@`.

Long helper table now complete:
- `N_LXMUL@` — multiply (fast-call CX:BX × DX:AX)
- `N_LDIV@` — signed div (stack args)
- `N_LUDIV@` — unsigned div (stack args)
- `N_LMOD@` — signed mod (stack args)
- `N_LXLSH@` — left shift
- `N_LXRSH@` — signed right shift (sar)
- `N_LXURSH@` — unsigned right shift (shr)

## `(int)long_var` truncate cast — single LOW word load (3B)

Fixture `3182-long-to-int-cast-obj`:

```c
int trunc(long v) { return (int)v; }
```

```
8b 46 04                       mov ax, [bp+4]    (LOW word, HIGH discarded)
```

Findings:
- Long-to-int truncate = single word load (3B).
- HIGH word silently discarded.

## `(long)signed_char` widening — `cbw + cwd` two-step sign-extend

Fixture `3183-char-to-long-cast-obj`:

```c
long widen(char c) { return (long)c; }
```

```
8a 46 04                       mov al, c
98                             cbw   (char → int, sign-ext AL→AX)
99                             cwd   (int → long, sign-ext AX→DX:AX)
```

Findings:
- Two-step sign-extension: `cbw` then `cwd`.
- 5 bytes total.
- For `(long)unsigned char`: `mov ah, 0; xor dx, dx` zero-extend variants.


## Unsigned long mod `a % b` — `N_LUMOD@` helper

Fixture `3185-ulong-mod-obj`:

```c
return a % b;   /* unsigned long */
```

Same 4-push pattern; helper name `N_LUMOD@`.

**Complete long div/mod family**:
- `N_LDIV@` — signed div
- `N_LUDIV@` — unsigned div
- `N_LMOD@` — signed mod
- `N_LUMOD@` — unsigned mod

## `(char)long` truncate — single `mov al, byte [mem]` (3B)

Fixture `3186-long-to-char-obj`:

```c
char low(long v) { return (char)v; }
```

```
8a 46 04                       mov al, byte [bp+4]   (low byte of LOW word)
```

Findings:
- Single byte load — all higher bits discarded.
- 3 bytes for the cast.

