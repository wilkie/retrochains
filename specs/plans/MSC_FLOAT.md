# MSC floating point — buildout plan

Getting `crates/msc/` from "float doesn't parse" to byte-exact `float`/
`double` codegen. This is the FP lever off the cross-compiler coverage
dashboard (≈52 fixtures, the `float/double` cluster). Floating point is a
**from-scratch FPU backend** — unlike the `long` work, which extended
existing infrastructure, FP needed a new literal pool, x87 encoding, the
8087-emulator linkage, and (for block 3+) FPU-stack-state tracking.

All byte patterns below were reverse-engineered from the `expected/msc/`
goldens (`HELLO.ASM` + the `HELLO.OBJ` bytes) under `cl /c /Fa /AS`.

## Status

Done and on `main` (byte-exact, zero regressions):

- **Block 1** (`d5a6be2fa`) — frontend. Lexer float literals
  (`3.0f`/`3.0`/`1e5`) → `Tok::Float(bits, double)`; `float`/`double`
  keywords; `Expr::FloatLit(bits, double)` (folds to the truncated int for
  `(int)` casts); `LocalSpec::float_(width, bits)` with `is_float`/
  `float_bits`; parser float local decls with literal initializers.
- **Block 2** (`f2345c160`) — basic decl/init codegen. See "Architecture".
  Flips 1670, 1672, 2132, 2139, 2151, 2193. MSC at 1980/3952.
- **Block 3a** (this commit) — `(int)<float/double param>` → `fld [bp+disp];
  call __ftol`, plus double/float **const-arg passing** at the caller
  (`fld $T; sub sp,N; mov bx,sp; fstp [bx]; fwait; call`, WithSlide frame +
  result temp). Flips 1678, 2143. MSC at 1982/3952. Also reworked OBJ record
  framing: `_TEXT` now emits one LEDATA per maximal contiguous function run,
  with a CONST float temp introduced by a later function flushed between the
  runs (mirrors the ASM's `_TEXT ENDS / CONST SEGMENT / _TEXT SEGMENT`
  interleaving). Each `_TEXT` LEDATA gets its own following FIXUPP.

The hard, non-obvious part (the emulator linkage) is done. Blocks 3+ build on
it but need an FPU-stack model.

## Architecture (as built in block 2)

### CONST literal pool
Distinct `(bits, width)` from all float locals are interned into a pool
placed in CONST **after** the strings, word-aligned, `width` bytes each
(MSC names them `$T20001` etc.). A `float` stores the f32 collapse of the
f64 value (4 bytes); a `double` stores the full f64 (8 bytes), little-endian.
Built in `build_obj` (`float_pool`/`float_offsets`); CONST length and the
CONST LEDATA include them.

### x87 instruction encoding (all WAIT-prefixed `9B`)
| op                         | bytes                         |
|----------------------------|-------------------------------|
| `fld dword [$T]`           | `9B D9 06 <off16>`            |
| `fld qword [$T]`           | `9B DD 06 <off16>`            |
| `fld dword [bp+d]`         | `9B D9 46 <d>` (mod01 r/m110) |
| `fld qword [bp+d]`         | `9B DD 46 <d>`                |
| `fstp dword/qword [bp+d]`  | `9B D9/DD 5E <d>`             |
| `fst  dword/qword [bp+d]`  | `9B D9/DD 56 <d>`             |
| `fwait`                    | `9B` (preceded by a parity NOP so the next statement is even-aligned) |

(`D9` = single/float, `DD` = double. modrm reg field: `/0`=fld `06/46`,
`/2`=fst `16/56`, `/3`=fstp `1E/5E`.)

### 8087-emulator linkage — the non-obvious part
Whenever the unit uses FP, MSC emits a fixed EXTDEF block **before**
`__acrtused` (`fp_extern_block` in `lib.rs`):

```
__fltused(type 0)  FJSRQQ  FISRQQ  FIERQQ  FIDRQQ  FIWRQQ   (FxxRQQ type 1)
```

and a per-instruction **marker fixup** on each x87 opcode's leading byte so
the linker can rewrite the site for emulation:

- `FixupKind::FloatMarker { target }` → FIXUPP `C4 <off> 56 <extdef_idx>`,
  offset = the instruction byte itself (no +1).
- `fld`/`fst`/`fstp` → `FIDRQQ`; the `fwait` site → `FIWRQQ`.
- The `fld`'s `off16` CONST reference uses `FixupKind::FloatLoad` → `C4 <off>
  9C` (same shape as `StrLoad`), resolved via the `(bits,width)` pool.

`__ftol` (float→int helper, block 3) is appended **after** the function-name
EXTDEFs (trailing), referenced by a normal self-relative `ExtCall`
(`84 <off> 56 <idx>`).

## What block 2 deliberately left out

FP arithmetic / casts with **constant operands are const-folded** by MSC, so
many "arithmetic" fixtures don't emit runtime FP ops at all:
- `(int)(a * b)` with const a,b → `mov ax, <int result>` (1751).
- `float r = a + b;` with const a,b → the *folded* float is materialized
  from a CONST temp (1671: `a+b`→a `4.0` `$T`), not a runtime `fadd`.

So `fadd`/`fmul`/`fild` only appear with **runtime** operands.

## Block 3+ — the remaining mechanisms

These need an **`FpuStack` model**: track whether `st(0)` currently holds a
live value, because MSC couples a float-local store with its consumer.

### Key coupling (decoded from 1671, 1675)
A float local that is immediately consumed by `(int)local` is stored with
`fst` (keep-on-stack, `…56…`) **not** `fstp`, and the `(int)local` is then
just `call __ftol` with **no reload** — it consumes the live `st(0)`. A
statement-at-a-time emitter can't produce this; the emitter must know the
top of stack is live.

### 3a — `(int)float` via `__ftol`
- Param/slot form (clean, no coupling): `fld [src]; call __ftol` → result in
  `AX`. Emit the call as `ExtCall { target: "__ftol" }`.
- Local-then-cast coupling: float-local init emits `fst` (live st(0)); the
  `(int)local` emits just `call __ftol`.
- Targets: 1671, 1673, 1675, 1678 (callee), and most `(int)<float>` returns.

#### Findings from starting 3a (important — no atomic target)
The conversion codegen is trivial, but **no fixture flips from it alone** —
each needs ≥1 coupled mechanism that isn't built yet:

1. **Float params are unsupported** — `int f(double d)` fails to parse
   (`parse_param_list` only accepts int/char/long/struct). Needed:
   `param_is_float` tracking, 4/8-byte param width, and a float-aware
   `param_disp` (a `double` param occupies 8 bytes, shifting later params —
   the same shape as `long_param_disp`). *Touching `param_disp` risks
   non-float regressions; gate the float-width path carefully.*
2. **Double/float const args** (caller side, 1678/2143 `main`):
   `fld QWORD PTR $T; sub sp,8; mov bx,sp; fstp QWORD PTR [bx]; fwait; call`.
   (4 bytes / `sub sp,4` for `float`.) Each x87 op carries its FIDRQQ/FIWRQQ
   marker; the `fld $T` carries a FloatLoad.
3. **`(int)<float local>` is coupled** (1671/1675): the local store is `fst`
   (keeps st(0) live) and the cast is just `call __ftol` — needs the
   `FpuStack` model. Also needs FP const-fold (1671 `a+b`, 1675 `(float)i`),
   so these are *not* the place to start.

**Smallest complete unit = 1678 + 2143** (`int dbl_to_int(double d){return
(int)d;}` + `main` calling it with a const): clean callee (`fld [bp+4]; call
__ftol`) + float params (#1) + double const arg (#2). That's the recommended
first 3a deliverable; the coupled-local form (#3) comes after the FpuStack
model.

#### `__ftol` EXTDEF placement (empirical, differs by shape)
`__ftol` is an `ExtCall` target, so the existing `helper_extern_order` places
it after `__chkstk`, before the function-name EXTDEFs. This **matches the
multi-function shape** (1678: `… __chkstk, __ftol, _dbl_to_int, _main`). But
the single-function coupled shape (1671) puts `__ftol` **after** `_main`
(trailing). So the placement rule isn't "always after `__chkstk`" — confirm
per shape; 1678/2143 work with the current helper placement, 1671 will need a
trailing-extern path.

### 3b — `(float)int` and runtime `fild`
- Const int → folded to a float CONST temp + `fld`/`fst` (1675).
- Runtime int → `fild` (load int, convert to FP). Encoding: `9B DB 06 …`
  (word) / `DF` forms — confirm from a runtime fixture's golden.

### 3c — FP compares
`fcomp`/`fcompp` then `fstsw ax; sahf` and a normal `jcc`. Targets: 1674
(`a<b`), 1754 (`==`), 1756 (`>=`). (The already-passing 2139/2151/2193 are
the const-folded compare-to-zero shapes — confirm the runtime path.)

### 3d — runtime arithmetic `fadd/fsub/fmul/fdiv`
Only for non-constant operands. Load operands to the FPU stack, op, store.
Memory-operand forms (`fadd dword [bp+d]` = `9B D8 46 …`, qword `DC`) and
the `faddp st(1),st`/etc. register forms — decode from a runtime fixture.

### 3e — float↔double conversion
`float f; double d = f;` and `(float)d` — the x87 loads/stores at one width
and stores at the other (`fld dword; fstp qword` and vice-versa). Targets:
1676, 1677.

### 3f — float/double args, returns, globals
- Double/float **args**: caller pushes the 4/8 bytes (or `fld`+`sub sp`/
  `fstp [sp]`); needed for 1678's `main` calling `dbl_to_int(3.5)`.
- Float **returns**: value returned on the FPU stack (`st(0)`).
- Float **globals**: `_DATA`/COMDEF storage at 4/8 bytes; loads/stores via
  `fld/fstp [g]` with a `GlobalAddr` fixup.

## Suggested order

3a (`__ftol`, params first, then the local coupling) → 3c (compares) →
3b (`fild`) → 3e (float↔double) → 3d (runtime arithmetic) → 3f (args/
returns/globals). Verify each sub-block byte-exact with
`xfix verify --compiler msc <fixture>` and regression-check the full
`xfix verify-all --compiler msc` after each.

## Code pointers
- `crates/msc/src/lex.rs` — float-literal lexing, `float`/`double` keywords.
- `crates/msc/src/parse.rs` — float local decls.
- `crates/msc/src/lib.rs` — `fp_extern_block`, the float pool, `FloatLoad`/
  `FloatMarker` fixups, EXTDEF/FIXUPP emission.
- `crates/msc/src/codegen/func.rs` — the float-local x87 init (block 2); the
  `FpuStack` model and `__ftol`/conversion emission land near here.
