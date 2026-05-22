# Runtime helpers (`N_LXMUL@` family etc.)

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

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

