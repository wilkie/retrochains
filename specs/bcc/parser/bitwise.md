# Bitwise operations

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## `cmp ax, 0` → `or ax, ax` peephole

Fixture `555` (`while ((c = g) > 0) ...`) — when the right
operand of a compare folds to 0 and the left has just been
loaded into AX, BCC emits the 2-byte `or ax, ax` instead of the
3-byte `cmp ax, 0`. Both set ZF/SF identically so the
subsequent conditional jump works the same. Added at the tail
of `emit_compare` (after all global/local fast paths).

## `if (g & K)` bit test

Fixture `569` (`int g; if (g & 1) ...`) — BCC uses the `test`
instruction to set ZF directly from a masked memory read,
avoiding the load-into-register-then-`and`-then-test path.
`emit_cond_test` now special-cases `BinOp::BitAnd` with an int
global LHS and a constant RHS: emit `test word ptr DGROUP:_g,
K` (`F7 06 lo hi imm_lo imm_hi`, 6 bytes), then the standard
`jne/je` pair. A new tasm IR variant `TestGroupSymImm16`
encodes it.

## `c & K` — `and ax, imm16` accumulator form

Fixture `609` (`char c; c = 15; return c & 4;`) — after `mov
al, byte ptr [bp-1]; cbw`, BCC emits `25 04 00` (`and ax,
imm16`, the AX-specific accumulator form). Our tasm parser
previously accepted only `and ax, <symbol-or-mem>` forms. Added
the `AndAxImm16` IR variant with encoding `25 lo hi` plus a
parser entry that fires when LHS is AX and RHS is a 16-bit
immediate. 3 bytes vs the 4-byte generic `81 E0 lo hi`.

## `x | K` / `x ^ K` — `or/xor ax, imm16` accumulator forms

Fixtures `611` (`return x | 8;`) and `612` (`return x ^ 3;`) —
mirrors of the batch-97 `and ax, imm16` fix. BCC uses the AX-
specific 3-byte accumulator forms: `0D lo hi` for OR and `35
lo hi` for XOR. Added `OrAxImm16` and `XorAxImm16` IR variants
with their parser entries.

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

