# Shifts and rotates

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## `*K` peephole — `shl ax, 1` for power-of-2 K

Fixture `592` (`int f(int x) { return x * 2; } int main(void) {
return f(g(3)); }`) — `emit_op_with_source` for `BinOp::Mul`
previously panicked for any immediate. BCC's pattern for `* K`
with K a small power of two is to unroll into `shl ax, 1`
repeated (no `imul` involved). Added that peephole; non-power-
of-2 immediates still panic with an explicit "no fixture"
marker (BCC's shape in that case is `mov dx, K; imul dx`).

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

## `-1` (80186): ENTER (`c8 NN 00 00`) + LEAVE (`c9`) + shift-imm (`c1 /4 imm8`) + push-imm8 (`6a imm8`)

Fixtures `2276` (-1 shift), `2277` (-1 fn w/
locals), `2278` (no -1 baseline) pin the 80186
instruction usage.

- `2276` (**-1 + shift by 4**): with `-1`,
  uses 3-byte `c1 /4 reg, imm8` form:
  ```
  c1 e0 04                  ; shl ax, 4 (80186 only)
  ```
  vs 8086's 4-byte `b1 04 / d3 e0` cl-form.
  Also uses ENTER/LEAVE for prologue/epilogue:
  ```
  c8 04 00 00              ; ENTER 4, 0 (push bp + mov bp,sp + sub sp,4)
  ; ... body ...
  c9                        ; LEAVE (mov sp,bp + pop bp)
  c3                        ; ret
  ```
- `2277` (**-1 + fn w/ locals**): each fn with
  locals uses ENTER/LEAVE:
  ```
  c8 06 00 00              ; ENTER 6, 0 (6 bytes of locals)
  push si                   ; save callee-saved reg
  ; ... body ...
  pop si
  c9 / c3
  ```
  main (no locals, just one push imm8 arg) uses
  plain push bp + mov bp, sp (3B) since ENTER 0
  would be 4B; LEAVE used in epilogue anyway:
  ```
  55 8b ec                  ; push bp / mov bp, sp
  6a 0a                     ; push 10 (80186 push imm8)
  e8 NN NN                  ; call fn
  pop cx
  5d c3                     ; (main keeps plain epilogue since LEAVE = pop bp)
  ```
- `2278` (**no -1, shift by 4**): standard 8086
  cl-form `b1 04 / d3 e0` (4 bytes).

**80186 (`-1`) instruction usage**:
| Instruction | 8086 form | 80186 form | Savings |
|-------------|-----------|------------|---------|
| Prologue w/ locals | `55 8b ec 83 ec NN` (6B) | `c8 NN 00 00` (4B) | 2B |
| Prologue no locals | `55 8b ec` (3B) | (still 3B, ENTER 0 = 4B) | 0 |
| Epilogue | `8b e5 5d` (3B) | `c9` (1B) | 2B |
| Push small imm | `b8 NN NN / 50` (4B) | `6a NN` (2B) | 2B |
| Push large imm | `b8 NN NN / 50` (4B) | `68 NN NN` (3B) | 1B |
| Shift by const N>1 | `b1 N / d3 /4` (4B) | `c1 /4 imm8` (3B) | 1B |
| Shift by 1 | `d1 /4 reg` (2B) | (same) | 0 |
| imul reg, imm | (N/A) | `69 /r imm16` (4B) | n/a |

**ENTER specifics** (80186):
- `c8 nn nn 00` — ENTER nn (local-bytes), 0 (nesting-level)
- Pushes BP, sets BP=SP, then SUB SP, nn
- 4 bytes total, vs 6 bytes for the 8086 sequence
- BCC always uses level=0 (no nested fn support)

**ENTER not used** when:
- No locals (saves 1 byte over ENTER 0)
- main / leaf fns with no frame needed

For the Rust reimplementation:
- Track 80186 target (-1 flag).
- For prologue with locals N > 0: emit ENTER nn.
- For epilogue: emit LEAVE.
- For shifts by const: emit `c1 /4 imm8`.
- For small imm pushes: emit `6a imm8`.

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


## `unsigned char >> K` — promotes to int, uses `sar` (fixture `2382`)

`unsigned char x = 0xC0; return x >> 1;` does NOT use `shr` on a
byte register. Instead, BCC follows C's default integer promotions:
the `unsigned char` value widens to `int` first (via `mov ah, 0`
zero-extend), and the shift is performed on AX with the **`int`
shift opcode `sar`**:

```
c6 46 ff c0             ; uchar x = 0xC0
8a 46 ff                ; mov al, x      (byte load)
b4 00                   ; mov ah, 0      (zero-extend uchar → int)
d1 f8                   ; sar ax, 1      ← /7 = sar, NOT /5 = shr
```

This is semantically equivalent here because after zero-extension AX
is always non-negative (high bit clear), so `sar` and `shr` produce
the same result. But the *encoding* is `d1 f8` (`/7`) not `d1 e8`
(`/5`).

Why? After promotion, the operand TYPE is `int` (signed), so BCC
selects the signed shift opcode based on type, not on the original
source type. The unsignedness of the original `char` is "forgotten"
once promotion happens.

So **`>>` on a promoted-to-int value always emits `sar`**, even when
the source was unsigned. Truly-unsigned shift right (`shr`) requires
the operand to be `unsigned int` at the point of the shift — promote
explicitly with `(unsigned int)x >> 1` to get `shr`.

(For comparison, `unsigned int >> 1` documented earlier emits `d1 e8`
= shr. The difference is whether the type IS unsigned int at the
shift site.)

## Shift by more than the bit width — emitted literally (fixture `2406`)

`int x; return x >> 24;` where `int` is 16 bits — BCC emits the
shift with `cl = 24` without folding or normalizing:

```
c7 46 fe 64 00          ; x = 100
8b 46 fe                ; mov ax, x
b1 18                   ; mov cl, 24     ← shift count > 16 (bit width)
d3 f8                   ; sar ax, cl
```

BCC does **no validation or folding** of over-large shift counts.
The C standard says shifting by ≥ the bit width is undefined
behavior; BCC trusts the input. On 8086 hardware, `sar` with
`cl=24` actually shifts 24 times (which, for signed int, eventually
fills with the sign bit).

For unsigned shift right (`shr`) with cl ≥ 16 on a 16-bit operand,
the result is implementation-dependent on the actual CPU (8086 vs
80186+ differ — the 80186 masks `cl & 0x1F`). BCC's emit is the
same either way; the runtime is what differs.

Confirms: shift codegen is purely structural (`d3 e0/e8/f8` etc.)
with the count passed verbatim. No range-clamping at compile time.

## Unsigned divide by 8 — three single-bit `shr` (not shifted by cl)

Fixture `2513-unsigned-div-pow2-obj`:

```c
unsigned int u;
u = 1000;
return u / 8;
```

```
55 8b ec 4c 4c                prologue + 2B local
c7 46 fe e8 03                u = 1000 (0x03e8)
8b 46 fe                      mov ax, u
d1 e8                         shr ax, 1
d1 e8                         shr ax, 1
d1 e8                         shr ax, 1
eb 00 8b e5 5d c3             epilogue
```

Findings:
- `unsigned int / 8` is unrolled as **three single-bit `shr ax, 1`**
  (each 2 bytes, total 6). BCC does NOT switch to the `shr ax, cl`
  form (which would be `b1 03; d3 e8` = 4 bytes total) — three
  unrolled shifts win on byte count for shift-by-3.
- **No `div` instruction**: unsigned divide by power-of-2 is pure
  shifts. Compare to *signed* `int / 8` (would need `idiv` because
  arithmetic right-shift of negative values rounds the wrong way
  per C semantics).
- The threshold "unroll vs `shr cl,N`" — at shift-3 it's 6 bytes
  unrolled vs 4 bytes via cl. BCC prefers the unroll here, so the
  decision is not pure byte-minimization. Likely the threshold is
  at 4+ shifts (where unrolled = 8 bytes > 4 bytes via cl).
- d1 /5 = `shr r/m16, 1` (single-bit form).


## Unsigned divide by 16 — switches to `shr ax, cl` form

Fixture `2519-unsigned-div-16-obj`:

```c
unsigned int u;
u = 1000;
return u / 16;
```

```
55 8b ec 4c 4c                prologue + 2B local
c7 46 fe e8 03                u = 1000
8b 46 fe                      mov ax, u
b1 04                         mov cl, 4
d3 e8                         shr ax, cl
eb 00 8b e5 5d c3             epilogue
```

Findings:
- At **shift count 4**, BCC switches from unrolled single-bit `d1 e8`
  to the **`mov cl, N; shr ax, cl`** form (`b1 04; d3 e8` = 4 bytes).
- The unroll/cl-form threshold is **4 shifts**:
  - Shift 1: 1 `d1 e8` (2B) — unrolled
  - Shift 2: 2 `d1 e8` (4B) — unrolled (ties cl-form 4B, picks unroll)
  - Shift 3: 3 `d1 e8` (6B) — unrolled (LOSES vs cl-form 4B! Yet
    BCC still unrolls — so this is NOT pure byte-min.)
  - Shift 4: cl-form (4B) — switches over.
- Hypothesis: BCC unrolls when count ≤ 3 to avoid CL-clobber, and
  spills to CL only when ≥4. So the rule is **"unroll if N ≤ 3, else
  use CL form"** — a fixed count threshold, not a byte-length
  threshold.
- This is worth catching as a peephole rule in our IR.


## Shift-by-8 (byte swap) — uses generic `mov cl, 8; shr ax, cl`

Fixture `2528-unsigned-shr-8-obj`:

```c
unsigned int u;
u = 0xABCD;
return u >> 8;
```

```
55 8b ec 4c 4c                prologue + 2B local
c7 46 fe cd ab                u = 0xABCD
8b 46 fe                      mov ax, u
b1 08                         mov cl, 8
d3 e8                         shr ax, cl
eb 00 8b e5 5d c3             epilogue
```

Findings:
- Shifting an unsigned int by exactly 8 (which is "move AH to AL,
  zero AH") uses the **generic cl-form** (`b1 08; d3 e8` = 4 bytes).
- BCC does NOT take the **byte-aware shortcut** that would be even
  shorter: `mov al, ah; mov ah, 0` (3 bytes: `8a c4; b4 00` no
  that's 4 bytes too; or `88 e0` mov al,ah = 2B + `b4 00` mov ah,0
  = 2B = 4B total — same length, but BCC ignores byte-register
  tricks).
- So the shift-codegen rule is uniform: ≤3 → unroll, ≥4 → cl-form,
  no special cases for shift-by-8 / shift-by-16 etc.


## Signed right-shift `s >> N` uses `sar` (NOT `idiv`)

Fixture `2540-signed-shr-obj`:

```c
int s;
s = -16;
return s >> 2;
```

```
55 8b ec 4c 4c                 prologue + 2B local
c7 46 fe f0 ff                 s = -16 (0xfff0)
8b 46 fe                       mov ax, s
d1 f8                          sar ax, 1              ; arithmetic shift
d1 f8                          sar ax, 1
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Signed **`s >> N`** uses **`sar`** (arithmetic right shift,
  opcode `d1 f8` with mod 11, opcode-ext 111, r/m 000=ax).
  Sar preserves the sign bit (MSB) for negative values, rounding
  toward -infinity.
- Crucial distinction from signed `s / 2^N`:
  - **`s >> N`** → `sar` (rounds toward -infinity)
  - **`s / 2^N`** → `cwd + idiv` (rounds toward zero)
  They give DIFFERENT results for negative non-multiples (e.g.
  `-7 >> 1 = -4` vs `-7 / 2 = -3`).
- The N≤3 unroll rule applies: `s >> 2` → 2× `d1 f8` (4 bytes).
  At N≥4 it'd switch to cl-form (`b1 N; d3 f8`).
- Operator table:

| type      | `>>` opcode | `/` opcode |
|-----------|-------------|-------------|
| unsigned  | shr (`d1 e8`) | shr or `shr cl` (pow-2 only) |
| signed    | sar (`d1 f8`) | idiv (always) |


## Variable shift count — `mov cl, [n_addr]` byte-load

Fixture `2543-shift-var-count-obj`:

```c
int x = 100;
int n = 3;
return x >> n;
```

```
55 8b ec 83 ec 04                 prologue + 4B locals
c7 46 fe 64 00                    x = 100               ; [bp-2]
c7 46 fc 03 00                    n = 3                 ; [bp-4]
8b 46 fe                          mov ax, x
8a 4e fc                          mov cl, byte [bp-4]   ; LOW BYTE of n
d3 f8                             sar ax, cl            ; signed shift
eb 00 8b e5 5d c3                 epilogue
```

Findings:
- For a **variable shift count**, BCC loads only the **low byte
  of the count into CL** via `mov cl, byte ptr [...]` (opcode `8a`).
  The high byte of the count variable is discarded — the 8086
  shift instructions only consume CL anyway, and shift counts in
  C are always small.
- Save: byte-load (`8a 4e fc` = 3B) vs word-load + register-rename
  (`8b 4e fc` 3B + use cl — same size but no penalty for byte).
- The shift opcode is `d3 f8` for signed (sar), would be `d3 e8`
  for unsigned (shr) — same operand encoding, different opcode-ext
  bit (5 vs 7).
- No special path for "shift count is a variable but its value is
  knowable at this site" — even if BCC could constant-fold n=3,
  it doesn't here because the assignment crosses sequence points.

