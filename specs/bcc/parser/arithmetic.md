# Arithmetic codegen (mul/div/mod/peepholes/identity folds)

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

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

## `c = a % b;` — store DX directly

Fixture `546` (`int a, b, c; ... c = a % b;`) — after `idiv`,
the remainder lives in DX. The generic arith-to-AX path tacks
on a `mov ax, dx` so callers can find the result in AX, but
when the destination is a memory slot we can `mov [c], dx`
directly and save 2 bytes. `emit_assign_local`'s stack-int
branch now special-cases `BinOp::Mod` to emit `cwd; idiv <b>;
mov [bp-N], dx` via a small helper `emit_arith_setup_for_mod`.

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

## `int x = -K;` — mask negative initializer to width

Fixture `632` (`int x = -5; return x;`) — `try_const_eval`
returns a u32 (`-5` becomes 0xFFFFFFFB = 4294967291 decimal),
which leaked through the stack-init `mov word ptr [bp-N],
{v}` write and produced an out-of-range imm. Fixed by masking
`v & 0xFFFF` (int) or `v & 0xFF` (char) at the stack-init
emit site. The global-init path already did this; the local-
init path didn't.

## R-to-L arg eval w/ comma op; local shadows global completely; `!x` = `neg/sbb r,r/inc` arithmetic idiom; const ternary folds; multi-return = multi-jmp to epilogue

Fixtures `2315`-`2320` cover assorted idioms and
edge cases.

- `2315` (**comma op in args, R-to-L**): args
  evaluated right-to-left in cdecl order, each
  arg fully evaluated (comma chain included)
  before next:
  ```
  ; sum2((x = 10, x), (x = 20, x))
  mov si, 5           ; x = 5 init
  mov si, 20          ; right arg: x = 20 first
  push si              ; push 20
  mov si, 10          ; left arg: x = 10
  push si              ; push 10
  call sum2           ; result = 30
  ; After: x = 10 (last written)
  ```
- `2316` (**local shadows global**): inner local
  `g` completely hides the global `g = 100`. The
  global is dead in this TU but still present in
  `_DATA`. Codegen never references it.
- `2317` (**`!x` arithmetic idiom**):
  ```
  ; !x lowers to:
  mov ax, x
  neg ax              ; CF = (x != 0)
  sbb ax, ax          ; ax = -CF (0 or -1)
  inc ax              ; ax = 1-CF (1 if x==0, 0 if x!=0)
  ```
  3 instructions (5 bytes). For `!!x`, apply
  twice (10 bytes).
- `2318` (**const ternary folds**): `(1 ? &a :
  &b)` resolves to `&a` at compile time. The
  unused branch (`&b`) doesn't emit any code:
  ```
  lea ax, [a]
  mov si, ax           ; p = &a directly
  mov word [si], 5     ; *p = 5
  ```
- `2319` (**multiple returns**): each `return X`
  emits `mov ax, X / jmp epilogue`. Function
  epilogue appears once at the end:
  ```
  ; if (x < 0) return -1;
  test x / jge skip / mov ax, -1 / jmp end
  ; if (x == 0) return 0;
  test x / jne skip2 / xor ax, ax / jmp end
  ; if (x < 10) return 1;
  cmp x, 10 / jge skip3 / mov ax, 1 / jmp end
  ; return 2;
  mov ax, 2
  end:
    (epilogue)
  ```
- `2320` (**static array large init**): all 20
  init values placed directly in `_DATA` at
  compile time. No runtime init code emitted —
  just direct memory loads via `a1`/`03 06`
  forms.

**`!x` lowering pattern in detail**:
The 3-instruction `neg / sbb / inc` idiom
converts any value to a normalized boolean (0 or
1):
- For x == 0: neg gives CF=0, sbb gives 0, inc
  gives 1
- For x != 0: neg gives CF=1, sbb gives -1, inc
  gives 0

For `!!x`, apply twice:
- After first `!`: ax = 0 (if x!=0) or 1 (if x==0)
- After second `!`: ax = 1 (if x!=0) or 0 (if x==0)
- Net: 1 if x non-zero, 0 if x zero — true boolean

Total cost: 10 bytes. An alternative `cmp x, 0 /
setne al / movzx ax, al` is 286+ only.

**Arg-evaluation order (final, confirmed)**:
1. Args pushed right-to-left (cdecl)
2. Each arg's expression is FULLY evaluated
   (including comma chains and side effects)
   BEFORE pushing
3. Result on stack after all pushes: arg1 at
   lowest stack addr, argN at highest
4. Callee reads args at `[bp+4]`, `[bp+6]`, ...

**Multi-return function shape**:
- Multiple `return X` statements all jmp to a
  single epilogue label
- Epilogue: pop callee-saved + leave + ret
- Optimizer may merge multiple "mov ax, K / jmp
  end" sequences if all share the same constant
  return value

For the Rust reimplementation:
- Right-to-left arg eval: emit pushes in reverse
  source order; each fully resolved.
- `!x`: emit `neg/sbb r,r/inc` 3-instr sequence.
- Multi-return: emit single epilogue with multiple
  jumps in.

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


## `x % 1` and `x << 0` fold to constants (fixture `2391`, `2392`)

The identity-fold table extends with two more rules:

**`x % 1` → `0`** (fixture `2391`): for any integer `x`, the modulo
of 1 is mathematically always 0. BCC folds this at parse time and
emits a bare `xor ax, ax` — neither `x` nor the divisor `1` appears
in the assembly:

```c
x = 42;
return x % 1;
```

```
33 c0                   ; xor ax, ax   ← entire `x % 1` collapses to 0
```

**`x << 0` → `x`** (fixture `2392`): shift-by-zero is the identity
operation. BCC elides the shift entirely:

```c
x = 100;
return x << 0;
```

```
8b 46 fe                ; mov ax, x    ← no shift instruction emitted
```

So the identity-fold catalog (extending earlier `x + 0` / `x - 0` /
`x | 0` / `x ^ 0` / `x / 1` / `x * 1` / `x * 0` rules):

| Source | Folds to |
|---|---|
| `x + 0`, `x - 0`, `x \| 0`, `x ^ 0` | bare `mov` |
| `x / 1`, `x * 1`, `x << 0`, `x >> 0` | bare `mov` |
| `x * 0`, `x % 1` | constant `0` (`xor ax, ax` or direct store) |
| `x & -1` | bare `mov` (already covered) |

Identity folds happen at parse time on integer literal RHS. The
folds gate on the RHS being an int literal of the specific value —
not on type or runtime knowledge of the LHS. So `x % var` where
`var` happens to be 1 at runtime does NOT fold (runtime modulo, full
idiv).

## `(0 == x)` commutes to `(x == 0)` — same bytes (fixture `2393`)

The equality comparison `0 == x` produces **byte-identical** OBJ to
`x == 0`. BCC normalizes the operand order at parse time, putting
the constant on the right so the standard `cmp m, imm` form
applies.

```c
if (0 == x) { r = 10; } else { r = 20; }
```

```
83 7e fe 00             ; cmp word ptr [bp-2], 0    ← x on left (memory), 0 on right (imm)
75 05                   ; jne else_branch
be 0a 00 eb 03          ; r = 10; jmp end
be 14 00                ; r = 20
```

Encoding details:
- `83 7e fe 00` = `cmp word [bp-2], imm8-sext-0` — the imm8-sext
  form for small constants.
- `75` = `jne` — branches on the inverted truth (if NOT equal, take
  else branch).

So `==` and `!=` are **commutative at the codegen level** in BCC:
`a == b`, `b == a`, `a == K`, `K == a` all collapse to the same
`cmp <non-const>, <const>` pattern when one operand is a constant.
This applies even for non-trivial constants (e.g., `(5 == x)` would
emit `cmp [m], 5`).

For ordered comparisons (`<`, `>`, `<=`, `>=`) the commute would
flip the predicate (`5 < x` is `x > 5`); not exercised in this
fixture but consistent with the rule.

## `if (x == x)` — NOT folded (fixture `2419`)

The comparison of a variable with itself is mathematically always
true (for integer types — for FP with NaN, false), but **BCC does
not fold it** at compile time:

```c
if (x == x) r = 1; else r = 0;
```

```
be 07 00                ; x = 7 (SI)
3b f6                   ; cmp si, si      ← compares register to itself
75 05                   ; jne else_branch  ← never taken (ZF always set after cmp r,r)
bf 01 00                ; r = 1
eb 02                   ; jmp end
33 ff                   ; r = 0  (dead code, never reached)
```

BCC emits the test, the conditional jump, AND the dead else-branch
body. Confirms:

- **No reflexive-comparison folding**: `x == x`, `x != x`, `x < x`,
  etc. are all emitted literally.
- **No dead-code elimination**: the else branch's `r = 0` is
  unreachable but still in the OBJ.

So the BCC optimizer's identity-fold catalog (documented elsewhere
in this file) is limited to literal-RHS folds (`x + 0`, `x * 1`,
etc.) — it doesn't recognize same-variable patterns even when
provably constant. Saves ~12 bytes here if it did.

A similar non-fold likely applies to `x + x - x`, `x * 0 + x`,
`(x|0)`, etc. — anywhere the constant-fold relies on
variable-identity rather than literal-zero/one.

## Compile-time integer overflow wraps silently (fixture `2427`)

`x = 30000 + 30000;` — the sum 60000 doesn't fit in a signed
16-bit int (max +32767). BCC's constant-fold pipeline computes the
result modulo 65536 and emits the wrapped value:

```
c7 46 fe 60 ea          ; mov [bp-2], 0xEA60   (= 60000 mod 65536; signed = -5536)
```

No warning, no clamp, no error — just silent two's-complement
wrap. Confirms:

- Compile-time arithmetic uses **modular 16-bit semantics** for
  int-typed expressions.
- BCC matches what would happen at runtime if the same operation
  ran on actual 8086 registers (no exception, just `add` produces
  CF set + wrapped result).
- The compile-time fold and runtime computation are byte-equivalent
  for this case: `30000 + 30000` and `mov ax, 0xEA60` are
  indistinguishable.

Larger temporaries (`u32` internally) presumably handle wider folds
during the parse-time evaluation, but the final result still narrows
to the destination type at the store. So `0x12345678` as a 32-bit
literal stored to int discards the high 16 bits (per long-to-int
narrowing-cast rules documented elsewhere).

## `!!x` — `!` applied twice, NOT collapsed to "is non-zero" (fixture `2478`)

`r = !!x;` is a common idiom for "convert any non-zero value to 1".
BCC emits the full `neg/sbb/inc` logical-NOT idiom **twice** — no
peephole collapses the double-negation:

```
8b 46 fe                ; ax = x
f7 d8                   ; neg ax           ← first !x
1b c0                   ; sbb ax, ax
40                      ; inc ax
f7 d8                   ; neg ax           ← second !x  (same sequence)
1b c0                   ; sbb ax, ax
40                      ; inc ax
89 46 fc                ; r = ax
```

Total: 12 bytes for `!!x` (= 2 × 6-byte `neg/sbb/inc`). A
hypothetical peephole could collapse this to "is x non-zero, 0 or
1" via:

```
; Hypothetical (not what BCC emits):
8b 46 fe                ; ax = x
f7 d8 1b c0 40          ; standard !x → produces 1 if x==0, else 0
f7 d8 1b c0 40          ; ... and we'd still need a second one!
```

Wait — there's no shorter sequence on the 8086 that produces 0/1
directly from "is non-zero" without a conditional branch. The
neg/sbb/inc idiom IS the optimal branchless form. So actually
`!!x` requires the double-application even on optimized compilers.

The fixture confirms: BCC doesn't collapse `!!x` to just "test x
and produce 0/1" via a different idiom — it always uses the
canonical neg/sbb/inc twice.

A semantically equivalent shorter form would be:

```
8b 46 fe                ; ax = x
0b c0                   ; or ax, ax  (test if zero)
75 03                   ; jne nonzero
33 c0                   ; xor ax, ax
eb 02                   ; jmp end
                        ; nonzero:
b8 01 00                ; mov ax, 1
```

= ~10 bytes with a branch. BCC's branchless 12-byte version is
slightly larger but contains no branches — typical-case faster on
8086 (no pipeline disruption on its 1-byte queue). Confirms BCC
prefers branchless idioms even when slightly larger.

## Signed `int / 8` — uses `cwd + idiv`, NOT `sar`

Fixture `2520-signed-div-8-obj`:

```c
int s;
s = -1000;
return s / 8;
```

```
55 8b ec 4c 4c                prologue + 2B local
c7 46 fe 18 fc                s = -1000 (0xfc18)
8b 46 fe                      mov ax, s
bb 08 00                      mov bx, 8
99                            cwd
f7 fb                         idiv bx
eb 00 8b e5 5d c3             epilogue
```

Findings:
- **Signed divide by 8 uses `cwd; idiv bx`** — NOT `sar ax, cl`.
  Even though sar would be 1-2 instructions vs the 4-byte `bx` setup
  + `cwd` + `idiv`, BCC defers to idiv because **sar rounds toward
  -infinity** for negative values while C requires round-toward-zero.
  Example: `-7 sar 3 = -1` (C/idiv) vs `-7 sar 3 = -1` here happens
  to match, but `-1 sar 1 = -1` (sar) vs `-1 / 2 = 0` (C) diverges.
- The codegen is byte-identical to `s / 9` or any non-pow2 divisor —
  signed divide gets no fast-path regardless of divisor.
- This explains pointer-diff (`2506`) using idiv too: a `p - q` is
  signed, so it can't shift even when sizeof is pow2.
- Compare to unsigned/pow2 (`2513`, `2519`) which uses shifts —
  unsigned shifts ARE safe because they round down which matches
  unsigned divide.

