# `float` / `double` codegen

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## `#` stringize, `##` token-paste, `defined()`; macro double-eval on arg side-effect; `#line` codegen-invisible; small-frame `dec sp` prologue

Fixtures `2291`-`2296` cover advanced preprocessor
mechanics and small-frame allocation.

- `2291` (**`#x` stringize**): converts macro arg
  to a string literal at preprocess time:
  ```
  #define STR(x) #x
  STR(hello)              → "hello"
  ```
  In `_DATA`: "hello\0" as the string contents.
- `2292` (**`a##b` token-paste**): concatenates
  two tokens into one identifier:
  ```
  #define CAT(a, b) a##b
  CAT(x, y)               → xy (one identifier)
  ```
  The fixture references `xy` which was declared
  separately — token-paste produces the literal
  identifier text.
- `2293` (**macro arg with side effect**): the
  classic SQUARE(++i) double-evaluation bug:
  ```
  #define SQUARE(x) ((x) * (x))
  SQUARE(++i)             → ((++i) * (++i))
  ```
  BCC emits two `inc` instructions and a mul.
  For initial i=0: i becomes 2; result = 1*2 = 2
  (or 2*1, order is UB but commutative). No
  warning emitted.
- `2294` (**macro precedence bug**): without
  parens around x in the macro body:
  ```
  #define DOUBLE(x) x * 2
  DOUBLE(3 + 4)           → 3 + 4 * 2 = 11
                            (NOT (3+4)*2 = 14)
  ```
  Confirms macro expansion is pure textual
  substitution — operator precedence applies at
  the call site.
- `2295` (**`defined()` operator**): valid only in
  `#if`/`#elif`:
  ```
  #if defined(FOO) && !defined(BAR)
  ; → 1 && 1 → 1 → take this branch
  ```
- `2296` (**`#line N "file"`**): codegen-invisible.
  Affects only error messages and (potentially)
  debug info. Same OBJ bytes as without it.

**Frame allocation byte-count optimization**:
- `sub sp, 2` (1 int local): `4c 4c` (dec sp / dec
  sp = 2 bytes, vs `83 ec 02` 3 bytes)
- `sub sp, 4` (2 int locals): `83 ec 04` (3 bytes,
  vs `4c × 4` = 4 bytes)
- `sub sp, N` (N up to 127): `83 ec N` (3 bytes)
- `sub sp, N` (N > 127): `81 ec NN NN` (4 bytes)

So **`dec sp / dec sp` only wins for N=2**. Above
that, the `83 ec` imm8 form is shorter.

**Macro pitfalls to test**:
- Arg side-effect double-eval (SQUARE(++i))
- Operator precedence (DOUBLE(3+4))
- Token-paste at preprocess time (CAT)
- Stringize at preprocess time (STR)
- `defined()` only in #if/#elif

**Full preprocessor operator catalogue**:
| Op | Where | Effect |
|----|-------|--------|
| `#x` | in macro body | Stringize: arg → "arg" |
| `a##b` | in macro body | Token-paste: → ab (single token) |
| `defined(X)` | in #if/#elif | 1 if X defined, 0 else |
| `\` | end of line | Continue logical line |
| `# directive` | start of line | Preprocessor directive |

For the Rust reimplementation:
- Preprocessor: implement #, ##, defined().
- Track macro arg lists; substitute on expansion
  (textual, NOT semantic).
- Small frame allocation: emit `dec sp × 2` for
  sub sp,2; else use `83 ec` imm8 form.

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

