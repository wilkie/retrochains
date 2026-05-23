# Calling conventions

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

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


## Static file-scope fn-ptr — `ff 16 disp16` (call moffs16 indirect) (fixture `2450`)

`static int (*fp)(int) = adder;` declares a file-scope (non-public)
function pointer initialized to the address of `adder`. Calling
through it emits the **`ff 16 disp16`** memory-indirect call form
(absolute memory operand):

```c
static int (*fp)(int) = adder;
return fp(5);
```

```
b8 05 00 50             ; push 5
ff 16 00 00             ; call near [_fp]   ← FIXUPP'd to fp's address in _DATA
59                      ; pop cx
```

Encoding `ff 16 disp16`:
- `ff` = single-operand op
- `16` = ModR/M `mod=00 reg=010 rm=110` = `/2 = call near` on
  `r/m=110 = [disp16]` (direct memory)
- `00 00` = disp16 = `_fp`'s address (FIXUPP'd at link time)

Distinct from:
- `ff 56 disp8` = `call near [bp+disp8]` — local fn-ptr on stack
- `ff 96 disp16` = `call near [bp+disp16]` — far stack local
- `ff 17` = `call near [bx]` — register-pointed
- `ff 16 disp16` = `call near [moffs16]` — **absolute global**

The `_DATA` section reserves 2 bytes for `_fp`, initialized via
FIXUPP to point at `_adder`. Since `_fp` is `static`, no PUBDEF —
only `_adder` is exported.

## `void` function — body and epilogue merged, no `jmp $+2`

Fixture `2511-void-fallthrough-ret-obj`:

```c
int g;
void touch(int v) {
  g = v;
}
```

```
55                         push bp
8b ec                      mov bp, sp
8b 46 04                   mov ax, [bp+4]       ; ax = v
a3 00 00                   mov [_g], ax         ; FIXUPP _g
5d                         pop bp
c3                         ret
```

Findings:
- Void fns still emit the **standard prologue (push bp; mov bp, sp)**
  and epilogue (pop bp; ret) — the no-locals-no-return contract
  doesn't elide BP setup.
- **No `eb 00` "jump to epilogue" placeholder** is emitted at end of
  body. The body's last instruction (the store to `_g`) is
  immediately followed by `pop bp; ret`. So:
  - int-returning fns: insert `eb 00` before epi (default-position).
  - void fns: body falls THROUGH into epi without any jmp.
  This is a distinguishing byte-level signature between void and
  int-return.
- AX is left holding the last computed value (v in this case) as a
  side-effect of `8b 46 04` — but the void calling convention
  promises nothing about AX, so callers don't observe.


## Extern call with one-arg → `pop cx` cleanup, NOT `add sp, 2`

Fixture `2522-puts-string-lit-obj`:

```c
int puts(const char *s);
int main(void) {
  puts("hi");
  return 0;
}
```

```
55 8b ec                    prologue
b8 00 00                    mov ax, 0                ; offset of "hi" in _DATA (FIXUPP)
50                          push ax                  ; arg1
e8 00 00                    call _puts               ; EXTDEF FIXUPP
59                          pop cx                   ; cleanup 1 arg (2B)
33 c0                        xor ax, ax              ; return 0
eb 00 5d c3                  epilogue
```

Findings:
- **Single-word cdecl cleanup is `pop cx`** (1 byte) — not
  `add sp, 2` (3 bytes). The popped value lands in CX (scratch).
  This is a 2-byte savings whenever exactly one word needs popping.
- For multi-word arg lists, BCC switches to `add sp, N` (more
  efficient than N×pop). To probe the exact threshold.
- `return 0` peephole: `xor ax, ax` (2B, opcode `33 c0`) instead of
  `mov ax, 0` (3B, `b8 00 00`).
- Extern function symbols are emitted via the EXTDEF record;
  string literals live in `_DATA` and carry a FIXUPP to relocate.
- The call uses near-call opcode `e8` (3B, disp16) — since we're
  in the small memory model the puts symbol resolves to a near
  intra-segment call. (Large/huge memory model would emit `9a` far
  call with seg:off.)


## 3-arg call — R-to-L push order, `mov ax,imm; push ax` for each, `add sp,6` cleanup

Fixture `2527-call-three-args-obj`:

```c
int add3(int a, int b, int c);
int main(void) {
  return add3(1, 2, 3);
}
```

```
55 8b ec                    prologue
b8 03 00                    mov ax, 3
50                          push ax            ; arg3 FIRST (R-to-L)
b8 02 00                    mov ax, 2
50                          push ax            ; arg2
b8 01 00                    mov ax, 1
50                          push ax            ; arg1 (closest to call)
e8 00 00                    call _add3         ; EXTDEF
83 c4 06                    add sp, 6          ; cleanup 3 words
eb 00 5d c3                 epilogue
```

Findings:
- **3 args = 12 bytes of push setup**: each arg costs 4 bytes
  (`mov ax, imm16` 3B + `push ax` 1B). BCC never uses `push imm16`
  because that's an 80186+ instruction; the target is 8086.
- **Multi-arg cleanup is `add sp, N`** — `83 c4 06` (3 bytes) for
  3 args. Confirms the threshold:
  - 1 arg → `pop cx` (1B)
  - ≥2 args → `add sp, N` (3B)
- Push order is strict **R-to-L cdecl**: arg3 pushed first lands at
  the bottom of the call's arg block; arg1 pushed last sits just
  below the return address.
- This is the canonical "external function call" shape — combines
  with EXTDEF for the symbol and a FIXUPP on the disp16 of `e8`.


## Function-pointer stored in local — indirect call via `call word ptr [bp-disp]`

Fixture `2544-fn-ptr-store-call-obj`:

```c
int square(int x) { return x * x; }
int main(void) {
  int (*fp)(int);
  fp = square;
  return fp(5);
}
```

main body:
```
55 8b ec 4c 4c               prologue + 2B local
c7 46 fe 00 00               fp = &square (FIXUPP _square, disp16=0)
b8 05 00                     mov ax, 5
50                           push ax                 ; arg
ff 56 fe                     call word ptr [bp-2]    ; INDIRECT
59                           pop cx                  ; cleanup 1-arg
eb 00 8b e5 5d c3            epilogue
```

Findings:
- Function-pointer **storage** in small memory model = 2-byte near
  pointer. Store via `c7 46 disp imm16` with FIXUPP for the target
  function symbol.
- **Indirect call via stack-resident fp** uses
  `ff 56 disp8` = `call word ptr [bp+disp8]` (3 bytes).
  ModR/M `56` = mod 01, opcode-ext 010 (call near absolute
  indirect), r/m 110 (bp+disp8).
- For a stack-resident fp at [bp-disp16], the encoding would be
  `ff 96 disp16` (4 bytes).
- Compare to a global fn-pointer at file scope (`2516`-style global
  storage), which used `ff 16 disp16` (`call word ptr [disp16]`).
- The fn-ptr-call shape preserves cdecl convention: caller pushes
  args R-to-L, callee returns via AX, caller cleans up the stack.


## Nested call chain `inc(dbl(inc(5)))` — strict push/call/pop sequence

Fixture `2552-nested-call-chain-obj`:

```c
int inc(int x) { return x + 1; }
int dbl(int x) { return x + x; }
int main(void) {
  return inc(dbl(inc(5)));
}
```

main body:
```
55 8b ec               prologue
b8 05 00               mov ax, 5
50                     push ax              ; arg of innermost inc(5)
e8 db ff               call _inc            ; (FIXUPP, near rel16)
59                     pop cx               ; cleanup 1 arg (popped value ignored)
50                     push ax              ; push inc-result as dbl's arg
e8 e1 ff               call _dbl
59                     pop cx               ; cleanup
50                     push ax              ; push dbl-result as outer inc's arg
e8 d1 ff               call _inc
59                     pop cx               ; cleanup
eb 00 5d c3            epilogue
```

Findings:
- Nested calls evaluate **innermost first** (the order arguments
  appear), strictly sequential.
- Between each call, the cycle is exactly:
  `push ax  → call  → pop cx  → push ax → ...`
- The just-returned AX from the previous call is the next call's
  argument. BCC doesn't try to optimize away the redundant
  pop-then-push: it ALWAYS cleans up cdecl-style after each call,
  then re-pushes for the next.
- This generalizes: any expression tree of single-arg call chains
  emits as N times `push ax; call; pop cx; ...` no matter how deep.
- Notable subtle bytes: `dbl(x)` was written as `x + x`, not `x * 2`,
  so BCC emits `mov ax, si; add ax, si` (literal interpretation).
  Multiplication-by-2 would emit `shl ax, 1` instead. **Source-level
  rewriting matters** — BCC does NOT canonicalize x+x and x*2.


## 8-arg call — 32 bytes of push setup, `add sp, 16` cleanup

Fixture `2559-call-eight-args-obj`:

```c
int add8(int a, int b, int c, int d, int e, int f, int g, int h);
int main(void) {
  return add8(1, 2, 3, 4, 5, 6, 7, 8);
}
```

```
55 8b ec                       prologue
b8 08 00 50                    push 8 (arg8)    ; first pushed (R-to-L)
b8 07 00 50                    push 7
b8 06 00 50                    push 6
b8 05 00 50                    push 5
b8 04 00 50                    push 4
b8 03 00 50                    push 3
b8 02 00 50                    push 2
b8 01 00 50                    push 1 (arg1)    ; last pushed (next to call)
e8 00 00                       call _add8 (FIXUPP)
83 c4 10                       add sp, 16       ; cleanup 8 args × 2B
eb 00 5d c3                    epilogue
```

Findings:
- 8 args = 8 × 4 bytes per push (mov+push) = **32 bytes of arg-prep**.
- **Cleanup: `add sp, imm8`** (3 bytes, `83 c4 imm8` — sign-extended).
  Works for any cleanup ≤ 127 bytes (63 args).
- For >127 bytes of args (>63 args), would switch to `add sp, imm16`
  (`81 c4 imm16`, 4 bytes).
- Per-call cleanup cost table:

| arg count | cleanup form         | bytes |
|-----------|----------------------|-------|
| 1         | `pop cx`             | 1B    |
| 2-63      | `add sp, imm8`       | 3B    |
| 64+       | `add sp, imm16`      | 4B    |


## Global fn-pointer initialized + indirect call — `ff 16 disp16`

Fixture `2607-global-fnptr-init-obj`:

```c
int dbl(int x) { return x + x; }
int (*op)(int) = dbl;
int main(void) {
  return op(7);
}
```

`_DATA` (2 bytes): `00 00` (FIXUPP for `_dbl`)

main body:
```
55 8b ec                       prologue
b8 07 00 50                    push 7
ff 16 00 00                    call word ptr [_op]   ; INDIRECT via global
59                             pop cx                ; cleanup 1 arg
eb 00 5d c3                    epilogue
```

Findings:
- Global fn-ptr indirect call uses **`ff 16 disp16`** (4 bytes):
  `call word ptr [moffs16]` with FIXUPP target = the fn-ptr global.
- ModR/M `16` = mod 00, opcode-ext 010 (call near indirect),
  r/m 110 (disp16-only). Distinct from local fn-ptr indirect:

| location of fn-ptr | call form           | bytes |
|--------------------|---------------------|-------|
| local (`[bp+disp8]`) | `ff 56 disp8`     | 3B    |
| local (`[bp+disp16]`) | `ff 96 disp16`   | 4B    |
| global             | `ff 16 disp16`      | 4B    |
| direct call (known fn) | `e8 rel16`     | 3B    |

- The global `_op` is initialized at link time via FIXUPP — the
  data bytes `00 00` get relocated to `_dbl`'s offset.


## Calling a void function — caller side identical to int-return call

Fixture `2623-void-fn-call-obj`:

```c
void noop(int x);
int main(void) {
  noop(5);
  return 0;
}
```

```
55 8b ec                       prologue
b8 05 00 50                    push 5
e8 00 00                       call _noop      ; EXTDEF FIXUPP
59                             pop cx          ; cleanup 1 arg
33 c0                          xor ax, ax (return 0)
eb 00 5d c3                    epilogue
```

Findings:
- Caller-side: byte-identical to a value-returning call. Push args,
  call, cleanup.
- The caller doesn't care about return type — it just doesn't read
  AX after the call. So void/non-void split is **purely on the
  callee side** (`2511` showed void callees skip the `eb 00`
  default-position jmp before pop bp).


## Zero-arg extern call — `call rel16` only, no cleanup

Fixture `2675-extern-fn-call-obj`:

```c
int getchar(void);
int main(void) {
  int c = getchar();
  return c + 1;
}
```

```
55 8b ec 4c 4c                 prologue + 2B local
e8 00 00                       call _getchar (EXTDEF FIXUPP)
89 46 fe                       c = ax           ; result already in AX
8b 46 fe 40                    mov ax, c; inc ax
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Zero-arg call has **minimum overhead**: just the 3-byte
  `e8 disp16` near-call. No push args, no cleanup.
- Result conventionally in AX. Caller can read directly.
- Same pattern for any zero-arg extern (puts(""), getchar(),
  etc.) and zero-arg user functions.


## 3-int call with VARIABLE args — `push word [bp+disp]` (3B each, NOT 4B)

Fixture `2688-three-int-call-obj`:

```c
int x = 1, y = 2, z = 3;
return triple(x, y, z);
```

```
ff 76 fa                       push word [bp-6]    ; z (1st pushed, R-to-L)
ff 76 fc                       push word [bp-4]    ; y
ff 76 fe                       push word [bp-2]    ; x
e8 00 00                       call _triple        (FIXUPP)
83 c4 06                       add sp, 6           ; cleanup 3 args
```

Findings:
- For **variable args** (loaded from memory), BCC uses **`ff /6
  r/m16`** = `push word [mem]` = 3 bytes per push.
- For **constant args** (like `triple(1, 2, 3)`), BCC emits 4 bytes
  per arg (`mov ax, imm16` + `push ax`, see `2527`).
- So **variable args save 1 byte each vs constants**. 3-arg call:
  - All-const: 12B push setup
  - All-var: 9B push setup
- ModR/M `76` = mod 01, opcode-ext 110 (push), r/m 110 (bp+disp8).
- Push order is unchanged: R-to-L per cdecl. Cleanup is
  `add sp, 6` for 3 args (3B).


## Empty void function `void nop(void) { }` — minimal 5-byte body

Fixture `2697-empty-void-fn-obj`:

```c
void nop(void) {
}
```

```
55                             push bp
8b ec                          mov bp, sp
5d                             pop bp
c3                             ret
```

Findings:
- **Absolute minimum function body: 5 bytes** (prologue 3B +
  epilogue 2B).
- Even an empty void function pays the BP setup. BCC always
  emits push bp/mov bp,sp/pop bp/ret.
- Void return → no `eb 00` placeholder before pop bp.
- No `sub sp, N` (no locals), no register saves (nothing used).

## Ternary as function arg — JOIN in AX, then push as arg

Fixture `2698-ternary-as-arg-obj`:

```c
int twice(int x);
return twice(a > b ? a : b);
```

```
55 8b ec 56 57                 prologue + push si, di
be 05 00 bf 07 00              a in si, b in di
3b f7                          cmp si, di
7e 04                          jle → ELSE
8b c6                          THEN: ax = si    (a)
eb 02                          jmp → JOIN
8b c7                          ELSE: ax = di    (b)
                               ; JOIN:
50                             push ax           ; push as call arg
e8 00 00                       call _twice (FIXUPP)
59                             pop cx            ; cleanup 1 arg
eb 00 5f 5e 5d c3              epilogue
```

Findings:
- Ternary as fn arg: both arms put result in AX, JOIN merges them,
  then `push ax` feeds the call.
- No intermediate spill or copy: the ternary's AX result is the
  arg for the call. Direct flow.
- Confirms: ternary-as-expression always uses AX as the value
  carrier; the post-JOIN code uses it for whatever next step needs.


## Passthrough call `return helper(v)` — 8-byte body

Fixture `2699-passthrough-arg-obj`:

```c
int wrap(int v) {
  return helper(v);
}
```

```
55 8b ec                       prologue
ff 76 04                       push word [bp+4]   ; push v
e8 00 00                       call _helper       (FIXUPP)
59                             pop cx             ; cleanup
eb 00 5d c3                    epilogue
```

Findings:
- The minimum-overhead "passthrough" wrapper: 8 bytes for the
  expression (push + call + pop) + 5 bytes for prologue/epi.
- AX is preserved as the wrap's return value (no extra copy).
- Variable-arg push via `ff 76 disp8` (3B) saves 1B over the
  constant-arg form.


## Char constant as call arg — `mov al, imm8; push ax` (3B, saves 1B vs int)

Fixture `2706-mixed-arg-types-obj`:

```c
int combine(int a, char b, int c);
return combine(10, 'X', 30);
```

```
b8 1e 00 50                    arg3 (int 30): mov ax,30 + push ax (4B)
b0 58 50                       arg2 (char 'X'): mov al,0x58 + push ax (3B)
b8 0a 00 50                    arg1 (int 10): mov ax,10 + push ax (4B)
e8 00 00                       call _combine
83 c4 06                       add sp, 6 (3 args)
```

Findings:
- **Char constant arg**: `mov al, imm8` (2B) + `push ax` (1B) = 3B.
- **Int constant arg**: `mov ax, imm16` (3B) + `push ax` (1B) = 4B.
- 1-byte savings per char-constant arg.
- The pushed AX has GARBAGE in AH (high byte) for char args.
  The callee reads only AL (`mov al, [bp+disp]`), so AH garbage
  is harmless.
- Stack frame slot for char arg is still 2 bytes (cdecl word
  alignment) — the wasted high byte is the cost of cdecl uniformity.


## **CORRECTION: 2-arg cleanup is `pop cx; pop cx` (2B)**, NOT `add sp, 4` (3B)

Fixture `2712-use-fn-result-obj`:

```c
int n = sum(3, 4);
return n + 1;
```

```
b8 04 00 50                    push 4
b8 03 00 50                    push 3
e8 00 00                       call _sum
59 59                          pop cx; pop cx     ; CLEANUP! (2B)
89 46 fe                       n = ax
8b 46 fe 40                    return n + 1
```

Findings:
- **2-arg cleanup uses TWO `pop cx` instructions** (2 bytes total),
  NOT `add sp, 4` (3 bytes). 1 byte saved.
- This **corrects the prior table** from `2522`/`2527`. Updated:

| arg count   | cleanup form         | bytes |
|-------------|----------------------|-------|
| 1           | `pop cx`             | 1B    |
| **2**       | **`pop cx; pop cx`** | **2B** |
| 3+ (sign-ext) | `add sp, imm8`     | 3B    |
| 64+         | `add sp, imm16`      | 4B    |

  The crossover from "pop chain" to "add sp" happens at 3 args.
  At 2 args, 2 × 1B pops still wins over the 3B add sp form.
- This is a beautiful nano-optimization — most function calls have
  1-2 args, and BCC saves 1B on the cleanup for 2-arg cases.


## Calling a global fn-pointer (uninitialized) — same `ff 16 disp16`

Fixture `2750-direct-fnptr-call-obj`:

```c
int (*global_fn)(int);    /* uninit → _BSS */
int main(void) {
  return global_fn(42);
}
```

```
b8 2a 00 50                    push 42
ff 16 00 00                    call word ptr [_global_fn] (FIXUPP)
59                             pop cx (cleanup 1 arg)
```

Findings:
- Calling a global function-pointer uses **`ff 16 disp16`** (4B)
  whether the pointer is initialized or uninitialized.
- Uninitialized `int (*op)(int);` lives in `_BSS` (2 bytes for the
  near pointer); initialized version goes to `_DATA` with FIXUPP.
- Either way, the CALL bytes are identical — the difference is
  purely in the symbol table / segment placement.
- BCC will only ACTUALLY work at runtime if something initializes
  the pointer first (here it would be 0 → null-call → crash).


## 2-arg fn-pointer call — same form as direct call

Fixture `2781-fnptr-2args-obj`:

```c
int (*op)(int, int);
int call(int a, int b) {
  return op(a, b);
}
```

```
ff 76 06                       push word [bp+6]    ; b
ff 76 04                       push word [bp+4]    ; a
ff 16 00 00                    call word ptr [_op] (FIXUPP)
59 59                          pop cx; pop cx     ; 2-arg cleanup (2B)
```

Findings:
- 2-arg fn-pointer call uses identical args/cleanup as direct call:
  push right-to-left, indirect call, 2-arg cleanup is **2× `pop cx`**
  (2B, per `2712` correction).
- The only difference from direct call is the call instruction
  (`ff 16 disp16` for indirect, `e8 disp16` for direct).


## Array-to-pointer decay at call site — `lea ax, [bp-N]; push ax`

Fixture `2787-arr-decay-arg-obj`:

```c
int sumof(int *p, int n);
int call_with_arr(void) {
  int a[3];
  /* ... a initialized ... */
  return sumof(a, 3);
}
```

```
b8 03 00 50                    push 3
8d 46 fa                       lea ax, [bp-6]   ; &a[0]
50                             push ax
e8 00 00                       call _sumof
59 59                          2-arg cleanup
```

Findings:
- Passing an array as an argument = **`lea ax, &a[0]; push ax`**
  (5 bytes). Array decays to a pointer to its first element.
- NO N_SCOPY@ struct-copy — the whole array isn't passed by value;
  only the pointer is.
- ModR/M `8d 46 disp8` for the lea = 3 bytes. Plus `push ax` (1B).
  Total: 4 bytes for the address push (vs. 6+ for pushing a struct
  by value).
- This is the C "array-to-pointer decay" rule at codegen level.


## Nested fn calls `f(g(v))` — `push v; call g; pop cx; push ax; call f; pop cx`

Fixture `2804-nested-fn-calls-obj`:

```c
int inc1(int x);
int inc2(int x);
int both(int v) {
  return inc1(inc2(v));
}
```

```
ff 76 04                       push v        ; arg for inc2
e8 00 00                       call _inc2
59                             pop cx        ; 1-arg cleanup
50                             push ax       ; result → arg for inc1
e8 00 00                       call _inc1
59                             pop cx
```

Findings:
- Nested calls chain via AX: inner call → push AX → outer call.
- Each call has its own cleanup (no merging).
- Total 13 bytes for a 2-call chain (8B inner + 5B outer push+call+pop).
- Generalizable: `h(g(f(v)))` would be 8 + 5 + 5 = 18 bytes.


## Zero-arg fn-ptr call — `ff 16 disp16` only (4 bytes, no push/cleanup)

Fixture `2818-fnptr-no-args-obj`:

```c
int (*hook)(void);
int trigger(void) {
  return hook();
}
```

```
ff 16 00 00                    call word ptr [_hook]  (FIXUPP)
```

Findings:
- Zero-arg fn-ptr call = **just the indirect call instruction**.
  No push (no args), no cleanup (sp unchanged).
- 4 bytes total for the entire call.
- Compare to 1-arg call: adds `push ax` (1B) + `pop cx` (1B) = +2B.
- Compare to direct fn call (zero-arg): `e8 disp16` (3B with FIXUPP).
  Fn-ptr is 1B more for the `ff 16` modr/m.


## Variable char passed to int-arg fn — byte load + cbw + push (promotion)

Fixture `2836-char-to-int-arg-obj`:

```c
int eat(int x);
int call(char c) {
  return eat(c);
}
```

```
8a 46 04                       mov al, c       (byte load)
98                             cbw             (sign-extend char→int!)
50                             push ax
e8 00 00                       call _eat
59                             pop cx
```

Findings:
- **Variable char argument** to an int parameter uses cbw to
  promote: byte load + cbw + push. The pushed AX has properly
  sign-extended AH.
- Contrast with **literal char arg** (`'X'`, `2706`): pushes
  `mov al, imm8; push ax` with **garbage in AH**.
- Difference: BCC's codegen path applies default-argument
  promotion when the source is a variable expression (which is
  type-evaluated), but takes a shortcut for char literals (which
  are folded directly into the immediate).
- Callee reading via `mov al, [bp+disp]` only cares about AL in
  both cases — so the garbage is harmless, but cbw is more
  "correct."


## Calling through fn-ptr PARAMETER — `ff 56 disp8` (3 bytes)

Fixture `2839-fn-takes-fnptr-obj`:

```c
int invoke(int (*op)(int), int v) {
  return op(v);
}
```

```
ff 76 06                       push v ([bp+6])
ff 56 04                       call word ptr [bp+4]   ; bp-rel indirect
59                             pop cx (1-arg cleanup)
```

Findings:
- Indirect call through a fn-ptr **parameter** uses `ff 56 disp8`
  (3 bytes) — 1 byte smaller than calling through a global fn-ptr
  (`ff 16 disp16`, 4 bytes).
- Saves a byte due to bp-relative disp8 vs absolute disp16.
- ModR/M `56 04` = mod 01, op-ext 010 (call near), r/m 110 ([bp+disp8]).


## `f(g(v,1), h(w,2))` nested calls multi-arg — right-to-left eval

Fixture `2853-nested-multi-arg-obj`:

```c
return op(op(v, 1), op(w, 2));
```

```
                               ; First: RIGHTMOST inner = op(w, 2)
b8 02 00 50                    push 2
ff 76 06                       push w
e8 00 00 59 59                 call op + 2-arg cleanup
50                             push ax (= outer arg 2)
                               ; Second: LEFT inner = op(v, 1)
b8 01 00 50                    push 1
ff 76 04                       push v
e8 00 00 59 59                 call op + cleanup
50                             push ax (= outer arg 1)
                               ; Outer: op(arg1, arg2)
e8 00 00 59 59                 call op + cleanup
```

Findings:
- Nested calls in cdecl evaluate **right-to-left at the outer
  level**: the rightmost outer arg is computed first.
- Each inner call's result is pushed as the outer's arg slot.
- Total ~30 bytes for the 3-call chain.
- AX is the conduit between each inner call and its consumer.


## `char_var → char_param` — NO cbw promotion (skips 1B)

Fixture `2938-char-pass-char-obj`:

```c
void recv(char c);
void send(char c) {
  recv(c);
}
```

```
8a 46 04                       mov al, c       (byte load)
50                             push ax         (NO cbw, AH garbage)
e8 00 00                       call _recv
59                             pop cx
```

Findings:
- When callee declared `char` parameter, BCC **skips the cbw**
  promotion that occurs for `char → int` param (`2836`).
- 4 bytes for the arg setup (vs 5B with cbw).
- Callee reads only AL anyway; AH garbage is harmless.
- Source-form rule: declare char param when caller has char to
  skip the promotion step.


## `ops[i](x)` — array of fn-ptrs dispatch via `call [bx + disp16]`

Fixture `2944-fn-ptr-arr-obj`:

```c
int (*ops[3])(int);
int dispatch(int i, int x) {
  return ops[i](x);
}
```

```
ff 76 06                       push x
8b 5e 04                       mov bx, i
d1 e3                          shl bx, 1
ff 97 00 00                    call word ptr [bx + _ops]  (FIXUPP, 4B!)
59                             pop cx (1-arg cleanup)
```

Findings:
- Function-pointer array dispatch via single indirect call
  through `[bx + disp16]`.
- ModR/M `97 disp16` = mod 10, op-ext 010 (call near), r/m 111
  ([bx + disp16]).
- 4 bytes for the call instruction with FIXUPP'd array base.
- Conceptually similar to dense-table switch dispatch but for
  fn-pointer arrays. Common idiom for vtable / state-machine code.


## 3rd int parameter — `[bp+8]`

Fixture `2953-third-int-param-obj`:

```c
int third(int a, int b, int c) {
  return c;
}
```

```
8b 46 08                       mov ax, [bp+8]   ; c
```

Findings:
- cdecl arg stack layout (caller pushes right-to-left):
  - 1st arg: `[bp+4]`
  - 2nd arg: `[bp+6]`
  - 3rd arg: `[bp+8]`
  - Nth arg: `[bp + 2*(N+1)]`
- Each arg occupies 2 bytes (word-aligned).
- Long args (4 bytes) bump the offset accordingly.

## 2 sequential calls with same arg — param promoted to si for reuse

Fixture `2956-fn-2nd-call-obj`:

```c
int both(int v) {
  f(v);
  return g(v);
}
```

```
8b 76 04                       mov si, v       (promote)
56                             push si
e8 00 00 59                    call f + pop cx
56                             push si         (REUSE si)
e8 00 00 59                    call g + pop cx
```

Findings:
- Param `v` is **promoted to si** so it can be reused across both
  calls without re-loading from `[bp+4]`.
- First call's result discarded (just `pop cx`); second's result
  carries through AX.


## Recursion `countdown(n - 1)` — backward `call rel16` with negative disp

Fixture `2994-recursion-obj`:

```c
int countdown(int n) {
  if (n == 0) return 0;
  return 1 + countdown(n - 1);
}
```

```
e8 ea ff                       call countdown   (disp16 = 0xFFEA = -22, backward)
```

Findings:
- Self-recursion = standard `call rel16` with **NEGATIVE disp16**
  (backward branch to function entry).
- Each recursive call gets its own stack frame (push bp, locals).
- BCC has no special handling for recursion — just emits the call
  with the right relative offset.


## `setup(); cleanup();` two zero-arg void calls — minimal back-to-back

Fixture `3012-two-extern-calls-obj`:

```c
setup();
cleanup();
```

```
e8 00 00                       call _setup
e8 00 00                       call _cleanup
```

Findings:
- Two zero-arg `void` calls = just two `call rel16` (3B each).
- NO push/pop, NO cleanup. Total 6 bytes for both.
- Minimum-cost call sequence.

## 3-arg fn-ptr call — `add sp, 6` cleanup (not 3× pop cx)

Fixture `3016-fnptr-3args-obj`:

```c
int compute(int (*op)(int, int, int), int a, int b, int c) {
  return op(a, b, c);
}
```

```
ff 76 0a                       push c
ff 76 08                       push b
ff 76 06                       push a
ff 56 04                       call word ptr [bp+4]
83 c4 06                       add sp, 6   (3-arg cleanup)
```

Findings:
- At 3 args, BCC uses `add sp, imm8` (3B) cleanup rather than
  3× `pop cx` (also 3B but 3 instructions).
- Same byte count, but fewer instructions = faster.
- Confirms threshold table:
  - 1 arg: `pop cx` (1B, 1 instr)
  - 2 args: 2× `pop cx` (2B, 2 instr)
  - **3 args: `add sp, 6` (3B, 1 instr)** ← switch point
  - 4+ args: `add sp, K` (3B, 1 instr)


## Fn-ptr local variable — assigned + called via `call word ptr [bp-disp]`

Fixture `3074-fnptr-local-obj`:

```c
int (*op)(int);
op = double_it;
return op(n);
```

```
c7 46 fe 00 00                 op = &double_it (mem-imm + FIXUPP, 5B)
ff 76 04                       push n
ff 56 fe                       call word ptr [bp-2]   (indirect via local)
59                             pop cx
```

Findings:
- Local fn-ptr assigned via mem-imm with FIXUPP (5B).
- Indirect call via `call word ptr [bp-disp]` (3B).
- Same call form as fn-ptr param (`2728`) but disp targets local.
- 1-arg cleanup via `pop cx`.

