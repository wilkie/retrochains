# Switch dispatch

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## Negative case label

Fixture `525` (`switch (x) { case -1: return 10; ... }`) —
`parse_switch`'s case head only accepted `IntLit` directly. It
now allows an optional leading `Minus` token and negates the
literal via `wrapping_neg` so the case value stays a u32 with
the same wrap-around semantics that `try_const_eval` produces
for `-1`. Codegen needed no change — switch comparison already
handles arbitrary u32 case values.

While integrating this, found a bug in `emit_assign_local`'s
stack-int immediate store: `try_const_eval` returns u32, so
`x = -1` was emitting `mov word ptr [bp-2], 4294967295`. Now
the path masks to `v & 0xFFFF` before formatting (matching the
already-correct char form). All prior fixtures still hit the
same byte output because their constants fit in 16 bits without
sign-extension; only the negative-literal path tripped this.

## Switch on a char scrutinee

Fixture `527` (`char c; c = 'B'; switch (c) { case 'A': ... }`)
— `emit_switch_chained`'s scrutinee load asserted Int locals
only. Char locals now load via `mov al, byte ptr [bp-N]; cbw`
(or `mov ah, 0` for uchar via `emit_widen_al`), promoting the
byte to AX before the chained `cmp ax, K / je` sequence. Case
values are 16-bit constants regardless of scrutinee type — BCC
uses `cmp ax, 0x42` even though the live value only occupies
AL.

## Switch on a non-ident scrutinee

Fixture `544` (`switch (x + 1) { case 1: ... }`) — when the
scrutinee isn't a bare ident, `emit_switch_chained` now routes
through `emit_expr_to_ax` and lets the result land in AX
directly. The chained-cmp+je sequence after the load is
unchanged. Ident scrutinees still hit the bespoke
char-widen/global-load shortcuts.

## Globals shared across fns via FIXUPP'd memops; typedef struct = anon+alias; fn returns ptr/fn-ptr in AX; **dense switch = CS-relative jump table** via `2e ff a7 disp16`

Fixtures `2333`-`2338` cover multi-fn globals,
typedef variations, ptr/fn-ptr returns, and the
JUMP TABLE optimization for dense switches.

- `2333` (**globals shared across fns**): both
  inc() and inc2() access `counter` via FIXUPP:
  ```
  ; inc():
  ff 06 00 00                  ; inc word [counter]
  
  ; inc2():
  83 06 00 00 02              ; add word [counter], 2
  
  ; main:
  call _inc / call _inc2 / call _inc
  a1 00 00                     ; mov ax, [counter]
  ```
  Global `counter` PUBDEF'd; other TUs can reference.
- `2334` (**typedef anonymous struct**): `typedef
  struct { ... } Point;` works as expected.
  Codegen identical to plain anonymous struct.
- `2335` (**fn returns ptr to struct**): pointer
  returned in AX as a 2-byte near pointer:
  ```
  ; struct P *get(void): returns &p_static
  b8 00 00                     ; mov ax, offset(p) FIXUPP
  
  ; Caller:
  call _get
  mov si, ax                    ; q = result
  mov ax, [si]                  ; q->x
  add ax, [si+2]                ; q->y
  ```
- `2336` (**fn returns fn ptr**): also a 2-byte
  near pointer in AX:
  ```
  ; In get_op:
  b8 00 00                     ; mov ax, offset(dbl) FIXUPP
  
  ; In main:
  call _get_op
  mov [fp], ax
  push 7
  ff 56 fe                     ; call near [fp]
  ```
- `2337` (**DENSE SWITCH = JUMP TABLE**): for 10
  contiguous cases (0-9), BCC emits a **jump
  table** stored inline in `_TEXT`:
  ```
  ; switch (x) with cases 0..9 + default
  
  mov bx, [x]
  cmp bx, 9
  ja default                    ; out-of-range
  shl bx, 1                     ; × sizeof(word) = 2
  2e ff a7 NN NN               ; jmp CS:[bx + table_offset]
                                ; ModR/M /4 = jmp near indirect
                                ; 0x2e = CS prefix override
                                ; rm=111 + disp16 = [BX+disp16]
  
  ; Case bodies (each ends with return -> jmp to epilogue):
  case0: mov ax, 100 / jmp end_switch
  case1: mov ax, 101 / jmp end_switch
  ...
  case9: mov ax, 109 / jmp end_switch
  default: xor ax, ax / jmp end_switch
  end_switch: (epilogue)
  
  ; Jump table (inline data in _TEXT):
  dw case0_offset
  dw case1_offset
  ...
  dw case9_offset
  ```
  Hugely efficient: O(1) dispatch regardless of N.
  
- `2338` (**1-element flex array idiom**): `char
  data[1];` at end of struct — used pre-C99 as
  variable-length-struct hack. Sized as 1 byte;
  total struct = sum of fields. Caller is
  expected to allocate more space and index
  `data[N]` beyond the 1 declared.

**Switch dispatch strategies (refined, FINAL)**:
| Pattern | Strategy |
|---------|----------|
| 1-3 cases (sparse) | Linear cmp/je chain |
| 4-7 cases (mixed) | Linear cmp/je chain |
| ≥ ~8 DENSE contiguous cases | **Jump table** with `2e ff a7 disp16` |
| Sparse but many cases (gaps) | Linear cmp/je or search table |
| Default-only | Body unconditionally |
| Multi-case same body | Shared target label |

**Jump table encoding details**:
- Range check: `cmp x, max / ja default`
- Index calc: `shl bx, 1` (× sizeof(word))
- Indirect jmp: `2e ff a7 disp16` (CS:[BX+disp16])
- Table: N word-sized offsets, inline in `_TEXT`
- CS override (`0x2e`) because table is in code segment

**Function return ABIs (final)**:
| Return type | Mechanism |
|-------------|-----------|
| int, near ptr, fn ptr | AX |
| long | DX:AX |
| Small struct ≤ 4B | DX:AX |
| Large struct > 4B | Hidden caller ptr + N_SCOPY@ |
| float/double | ST(0) (FPU stack top) |
| void | (no return) |

For the Rust reimplementation:
- Track contiguous case range during switch
  parsing; emit jump table if ≥ ~8 dense cases.
- CS-relative table emission: append to `_TEXT`
  after case bodies.
- Same indirect-jmp encoding (`2e ff a7 disp16`).

## `pascal` = UPPERCASE name + L-to-R push + `ret N` callee cleanup; `interrupt` = save all + IRET; `cdecl` = default

Fixtures `2246` (pascal), `2247` (interrupt),
`2248` (explicit cdecl) pin the calling
conventions.

- `2246` (**pascal**):
  - Symbol: **`PSUM`** (UPPERCASE, NO underscore
    prefix) — vs cdecl `_psum`
  - Args pushed **LEFT-TO-RIGHT** at call site
    (vs cdecl R-to-L)
  - **Callee cleans up** via `ret N`:
    ```
    c2 04 00         ; ret 4 (callee pops 4 bytes)
    ```
  - Caller emits NO cleanup after call
- `2247` (**interrupt**): completely different
  function structure:
  ```
  ; Prologue (9 pushes):
  push ax / push bx / push cx / push dx
  push es / push ds
  push si / push di / push bp
  
  ; Fix up DS to point at this module's data:
  mov bp, DGROUP
  mov ds, bp
  
  ; Standard frame setup:
  mov bp, sp
  
  ; ... body ...
  
  ; Epilogue (9 pops in reverse):
  pop bp / pop di / pop si
  pop ds / pop es
  pop dx / pop cx / pop bx / pop ax
  
  ; IRET (not ret):
  cf
  ```
- `2248` (**explicit cdecl**): byte-identical to
  default — `_csum`, R-to-L push, caller cleanup.
  The keyword is a no-op confirming default.

**Calling convention summary** (final):
| Convention | Name | Push order | Cleanup | Return |
|------------|------|------------|---------|--------|
| `cdecl` (default) | `_funcname` | R-to-L | Caller (`add sp` / `pop cx`) | `c3` (ret near) or `cb` (ret far) |
| `pascal` | `FUNCNAME` (upper, no `_`) | L-to-R | Callee (`c2 NN 00` ret N) | `c2 NN 00` |
| `interrupt` | `_funcname` | (no args usual) | (full reg save/restore) | `cf` (IRET) |

**Pascal symbol naming**: the symbol table entry
for `psum` declared as `pascal` shows `PSUM`
(uppercase). Linker likely matches case-
sensitively, so callers must agree (typically
both sides have the same `pascal` declaration).

**Interrupt fn details**:
- Saves AX, BX, CX, DX (data regs)
- Saves ES, DS (segment regs)
- Saves SI, DI (index regs)
- Saves BP (frame ptr)
- Restores DS to module's DGROUP (since interrupts
  fire with caller's DS)
- Returns with IRET (pops IP, CS, flags)

For the Rust reimplementation:
- Track calling convention attribute per fn.
- Pascal: emit UPPERCASE PUBDEF symbol + `c2 NN
  00` ret + L-to-R caller pushes.
- Interrupt: emit save-all prologue + IRET +
  DGROUP DS fixup.
- cdecl: default; no special handling.

## Switch only-default = unconditional body; fall-through = linear bodies; multi-case shares one target

Fixtures `2222` (switch default-only), `2223`
(fall-through), `2224` (multi-case shared body)
cover switch-statement edge cases.

- `2222` (**switch with only default**): no cases
  to check — default body executed unconditionally:
  ```
  ; switch (x) { default: r = 99; break; }
  ; (no jcc structure, just the body)
  mov word [r], 99
  ; jmp +0 (no-op for break/end)
  ```
- `2223` (**fall-through cases**): each case body
  emitted in order; no inter-case jmp; control
  flows through:
  ```
  ; switch(x) { case 1: r+=10; case 2: r+=20; case 3: r+=30; }
  cmp ax, 1 / je case1
  cmp ax, 2 / je case2
  cmp ax, 3 / je case3
  jmp end                 ; no default — fall out
  case1: add si, 10
  case2: add si, 20       ; falls through from case1
  case3: add si, 30       ; falls through from case2
  end:
  ```
  For x=1, all three case bodies execute: r = 10+20+30 = 60.
- `2224` (**multi-case shared action**): all `case
  N:` labels for the same action point to a
  single shared target — body emitted once:
  ```
  ; switch(x) { case 1: case 2: case 3: r=100; break; default: r=0; }
  cmp ax, 1 / je shared
  cmp ax, 2 / je shared    ; same target
  cmp ax, 3 / je shared    ; same target
  jmp default              ; (no match)
  shared:
    mov si, 100
    jmp end                 ; break
  default:
    xor si, si
  end:
  ```

**Switch codegen tactics summary** (refined):
| Pattern | Tactic |
|---------|--------|
| Default only | Emit body unconditionally |
| Fall-through | Linear bodies, no inter-case jmp |
| Multi-case same action | Shared label target |
| Dense cases (≥ 4 contiguous) | Jump table |
| Sparse cases (≤ 3 or non-contiguous) | Linear cmp/je chain |
| Sparse but ≥ 4 cases | Search table |

For the Rust reimplementation:
- Default-only switch: bypass jcc; emit body.
- Multi-case same action: dedup labels.
- Fall-through: no extra jmp between case bodies.

## Switch on long: two-phase search-table; struct-of-6 uses `imul`; signed `>=` uses `jl`

Fixtures `1913` (switch on long), `1914` (array
of 6-byte struct), `1915` (signed `>=` cmp) cover
remaining shapes.

- `1913` (**switch on long = 2-phase search**):
  switch on `long x` (32-bit) uses the linear-
  search-table strategy with **two-phase compare**
  per iteration:
  ```
  loop_top:
  cs: mov ax, [bx]          ; load case.lo
  cmp ax, [temp.lo]
  jne L_next                 ; lo mismatch → next
  cs: mov ax, [bx+6]         ; load case.hi (table offset = 6 bytes for 3 cases × 2)
  cmp ax, [temp.hi]
  je L_found                 ; both match → use this case
  L_next:
  inc bx / inc bx
  loop loop_top              ; CX-- + jump if non-zero
  jmp L_after
  L_found:
  cs: jmp [bx + 12]          ; offset to body-target table (12 = 2 × 6)
  
  ; THREE tables in code segment:
  case_lo_table:     dw 1, 2, 3
  case_hi_table:     dw 0, 0, 0
  body_offset_table: dw L_c1, L_c2, L_c3
  ```
  Long switches **always use search-table**
  (never jump-table) — a 32-bit-indexed jump
  table would be impractical.
- `1914` (**non-pow2 struct stride uses imul**):
  `struct R {int a; int b; int c;}` (sizeof = 6)
  arrays use **`imul`** for index computation:
  ```
  mov ax, i / mov dx, 6 / imul dx     ; ax = i * 6
  lea dx, [base + field_offset]       ; field-specific base
  add ax, dx                          ; final addr
  mov bx, ax / mov [bx], ...
  ```
  No CSE across fields in the same iteration —
  each field access recomputes `i * 6` via imul.
  Pow2 strides use shl (cheaper); non-pow2
  always uses imul.
- `1915` (**signed `>=` uses `jl`**): false-
  branch jcc for `a >= b` is `jl` (`0x7C`,
  signed jump-if-less). Inverse of `>=` is `<`,
  hence `jl`.

**Complete signed-cmp jcc table** (false-branch):
| Op | False-jcc | Opcode |
|----|-----------|--------|
| `<` | `jge` | 7D |
| `<=` | `jg` | 7F |
| `>` | `jle` | 7E |
| `>=` | `jl` | 7C |
| `==` | `jne` | 75 |
| `!=` | `je` | 74 |

For the Rust reimplementation:
- Long switch: always emit search-table with two-
  phase compare. Three tables (case.lo, case.hi,
  body-target) of `N * 2` bytes each in code
  segment.
- Array of struct with non-pow2 stride: `mov ax,
  idx / mov dx, sizeof / imul dx`.
- Track operand signedness, choose jcc per
  signedness × operator matrix.

## Default doesn't count toward N; 2 cases linear; imm16 base uses `81 eb`

Fixtures `1910` (3 cases + default), `1911` (2
cases only), `1912` (base 200 imm16) refine the
switch case-count and base-encoding rules.

- `1910` (**3 cases + default = linear chain**):
  the **4-case threshold counts only explicit
  cases** — default doesn't push it over. 3
  explicit cases + default still uses linear
  cmp/je with `jmp L_default` as final fallback.
- `1911` (**2 cases = linear**): confirms 2 cases
  uses cmp/je chain (well below threshold).
- `1912` (**base 200 → imm16 sub**): when base
  doesn't fit imm8-sext, BCC uses **`81 eb imm16`**
  (sub r16, imm16, 4 bytes):
  ```
  mov bx, [x]
  sub bx, 200            ; 81 eb c8 00 (imm16 form, 4 bytes)
  cmp bx, 3
  ja L_after
  ; ... rest same as before
  ```

**Final base-normalize encoding hierarchy**:
| Base value | Encoding | Bytes |
|------------|----------|-------|
| 0 | (omitted) | 0 |
| 1 | `dec bx` | 1 |
| -128..127 (≠ 0,1) | `83 eb imm8` | 3 |
| else | `81 eb imm16` | 4 |

For the Rust reimplementation:
- Threshold for jump-table: count explicit cases
  (NOT default).
- Base-normalize: 0 omit, 1 dec, fits imm8-sext
  sub-imm8, else sub-imm16.

## NEW: 3rd switch strategy — linear-search table for sparse N ≥ 4 cases (uses `loop` insn)

Fixtures `1907` (base 0), `1908` (huge-gap),
`1909` (negative base) uncover a **third switch
codegen strategy** plus boundary refinements.

- `1907` (**base = 0 omits subtract**): when the
  lowest case value is 0, BCC **omits the
  normalization step** entirely:
  ```
  mov bx, [x]
  cmp bx, 3            ; bounds (no sub needed, x already 0-based)
  ja L_after
  shl bx, 1
  cs: jmp [bx + table]
  ```
  Most compact form (4 fewer bytes than base≠0).
- `1908` (**large gap → linear-search TABLE**):
  cases 1, 2, 3, 50 (huge gap) use a **brand new
  strategy** — neither jump table nor cmp/je chain:
  ```
  mov [tmp], x
  mov cx, 4              ; loop count = N cases
  mov bx, value_table    ; ptr to case-value table
  loop_top:
  cs: mov ax, [bx]       ; load case value
  cmp ax, [tmp]
  je L_found             ; match → use this case
  inc bx / inc bx        ; bx += 2 (next value)
  loop loop_top          ; CX-- + jump if non-zero
  jmp L_default          ; no match
  L_found:
  cs: jmp [bx + 8]       ; bx points into value_table;
                          ; +8 = offset to corresponding target_table entry
  
  ; two parallel tables in code segment:
  value_table:  dw 1, 2, 3, 50      ; the case values
  target_table: dw L_c1, L_c2, L_c3, L_c50  ; corresponding bodies
  ```
  The `bx + 8` trick: BX iterates the value-
  table; once matched, the matching index also
  identifies the target-table entry at offset
  `+sizeof(value_table)` (= 8 bytes for 4 cases).
  
  Uses the **`loop` instruction** (`e2 rel8`) for
  compact CX-counted iteration. This is a
  **third codegen strategy** for switch:
  - Linear cmp/je chain (for ≤ 3 cases or sparse-
    few)
  - Jump table (≥ 4 cases, range small)
  - **Linear-search value table** (≥ 4 cases,
    range large)
  
  Threshold for jump-table vs linear-search-table
  not yet pinned, but ≥ ~10× more values than
  cases probably triggers the switch.
- `1909` (**negative base**): cases -2 to 1 (base
  -2) uses `sub bx, -2` encoded as `83 eb fe`
  (imm8-sext to 0xFFFE = -2). Negative bases work
  correctly via sign-extension semantics of
  imm8-sext.

**Three switch strategies summary**:
| Pattern | Detection | Lowering |
|---------|-----------|----------|
| Linear chain | N ≤ 3 OR sparse with small N | cmp ax, K / je ... per case |
| Jump table | N ≥ 4, range small | sub/dec normalize, bounds, shl, cs:jmp [table] |
| Search table | N ≥ 4, range large | cx-count loop over value_table, cs:jmp [bx+offset] |

For the Rust reimplementation:
- Add the linear-search-table strategy to switch
  lowering.
- Base = 0: omit normalization step.
- Base < 0: use `sub bx, K` (imm8-sext handles
  negative).

## Switch jump-table: gaps point to L_after; default = `ja L_default`; non-1 base uses `sub bx, K`

Fixtures `1904` (4 cases with gap), `1905` (4
cases + default), and `1906` (cases starting at
base 5) refine the jump-table mechanism.

- `1904` (**gap in cases**): `case 1, 2, 4, 5`
  (with case 3 missing) **still uses jump table**
  — bounds check is `cmp bx, 4` (range = max -
  min = 5-1 = 4), table has **5 entries**:
  | Slot | Case | Target |
  |------|------|--------|
  | 0 | 1 | case 1 body |
  | 1 | 2 | case 2 body |
  | 2 | (missing 3) | **L_after** |
  | 3 | 4 | case 4 body |
  | 4 | 5 | case 5 body |
  
  Missing-case slots **point to L_after**
  (past the switch) — equivalent to "no match,
  fall through". So small gaps don't disable the
  jump-table approach.
  
  Refined threshold: N ≥ 4 distinct cases AND
  the case-value range is dense enough (gap-
  tolerance threshold not yet pinned).
- `1905` (**4 cases + default**): table has 4
  entries (one per explicit case); the **bounds
  check's `ja` targets the default body**
  directly:
  ```
  cmp bx, 3
  ja L_default       ; out-of-range → default
  shl bx, 1
  cs: jmp [bx + table]
  ```
  Default body laid out after case bodies, with
  its own break.
- `1906` (**non-1 base case value**): `case 5, 6,
  7, 8` uses **`sub bx, 5`** (`83 eb 05`, 3
  bytes) instead of `dec bx`. The bounds check
  and jump-table protocol are otherwise
  identical:
  ```
  mov bx, [x]
  sub bx, 5            ; normalize to 0-based
  cmp bx, 3            ; bounds (range = 3)
  ja L_after
  shl bx, 1
  cs: jmp [bx + table]
  ```
  
  Encoding choice:
  - base = 1: `dec bx` (1B)
  - base = K (≠ 1, fits imm8-sext): `sub bx, K`
    (3B)
  - base = K (imm16 only): `sub bx, K` (4B via
    `81 eb`)

For the Rust reimplementation:
- Switch jump-table mechanism:
  1. Compute base = min case value
  2. Subtract base from input (dec for base=1, sub
     bx, base otherwise)
  3. Bounds check `cmp bx, (max-min)` + `ja
     <default or L_after>`
  4. `shl bx, 1`
  5. `cs: jmp [bx + table]`
  6. Table fills gaps with L_after (or default if
     present)
- Default present: `ja` target = default body
- Default absent: `ja` target = L_after

## Switch jump-table threshold pinned: N ≥ 4 contiguous cases → jump table

Fixtures `1901` (4 cases), `1902` (5 cases), and
`1903` (6 cases) pin down the exact threshold for
jump-table-based switch lowering.

All three use **jump tables** with the same
template:
```
mov bx, [x]
dec bx                  ; bx -= 1 (lowest case)
cmp bx, N-1             ; bounds check (N = case count)
ja L_after              ; out-of-range
shl bx, 1               ; word index
cs: jmp [bx + table]    ; indirect jump

; data segment (in code):
table: dw L_case1, L_case2, ..., L_caseN
```

The bounds check uses N-1 as the upper limit:
- 4 cases: `cmp bx, 3`
- 5 cases: `cmp bx, 4`
- 6 cases: `cmp bx, 5`
- 8 cases: `cmp bx, 7`

Combined with earlier findings:
| N cases | Codegen |
|---------|---------|
| 3 (1894) | Linear cmp/je chain |
| 4 (1901) | Jump table |
| 5 (1902) | Jump table |
| 6 (1903) | Jump table |
| 8 (1898) | Jump table |
| 3 sparse (1897) | Linear cmp/je chain |

**Exact threshold**: **N ≥ 4 contiguous cases →
jump table**. For N ≤ 3 OR sparse cases, linear
chain is used.

The contiguity requirement is critical: even with
N ≥ 4, if cases are sparse (e.g., 10, 100, 1000),
the jump table would have huge gaps and BCC falls
back to linear chain.

For the Rust reimplementation:
- Analyze case-value distribution:
  - If N ≥ 4 AND cases form a contiguous range
    (max - min == N - 1): emit jump table
  - Else: emit linear cmp/je chain
- Jump-table emission:
  - `mov bx, [x] / sub bx, min / cmp bx, N-1 /
    ja default / shl bx, 1 / cs: jmp [bx +
    table_offset]`
  - Table in code segment with N word entries
- Update [[batch-525-switch]] reference: that
  earlier note's "always linear" claim was wrong
  for N ≥ 4 contiguous.

## CORRECTION: switch with 8 contiguous cases uses JUMP TABLE; char-switch promotes via cbw; static arr in `_DATA`

Fixtures `1898` (8 contiguous cases), `1899`
(switch on char), and `1900` (static array
lookup) overturn the earlier "all switches use
linear chain" claim and document table-driven
switch.

- `1898` (**8 cases use JUMP TABLE** — overturns
  prior finding):
  ```
  mov bx, [x]
  dec bx                ; bx = x - lowest_case (1 here)
  cmp bx, 7             ; bounds: 8-1
  ja L_after            ; out-of-range → skip switch
  shl bx, 1             ; bx *= 2 (word index)
  cs: jmp [bx + table]  ; INDIRECT JUMP via table
  
  ; data in code segment:
  table: dw L_case1, L_case2, L_case3, ..., L_case8
  ```
  This **overturns** the claim from
  [[batch-525-switch]] that BCC always uses
  linear chain. **BCC uses a jump table for
  dense contiguous cases with sufficient count**.
  
  Threshold for jump table is **somewhere
  between 4 and 8** cases (not yet pinned down).
  Sparse cases (1897) and few cases (1894 with
  3) still use linear chain.
  
  The jump table is stored **in the code segment**
  (CS:), with each entry being a 2-byte offset
  to the case body. The bounds check before the
  table dispatch handles out-of-range values
  (including those past the highest case).
- `1899` (**char-switch promotes via cbw**):
  switch on a `char` variable emits:
  ```
  mov al, [c]
  cbw                  ; sign-extend AL → AX
  ; then cmp ax, K / je ... for each case
  ```
  Char arg is promoted to int (signed sign-
  extension) before the switch's cmp. With 3
  cases here, linear chain is used (below jump-
  table threshold).
- `1900` (**static array in `_DATA`**): `static
  int table[5] = {10, 20, 30, 40, 50}` is
  emitted in `_DATA` with the initial values:
  ```
  table: dw 10, 20, 30, 40, 50    ; 0a 00 14 00 1e 00 28 00 32 00
  ```
  Accessed via `mov ax, [bx + table]` (with
  FIXUPP for table's address). Same codegen
  as a static array at file scope.

**Revised switch lowering rules**:
| Case shape | Codegen |
|------------|---------|
| Few cases (≤ ~4) | Linear cmp/je chain |
| Many sparse cases | Linear cmp/je chain |
| Many DENSE contiguous cases | Jump table |

For the Rust reimplementation:
- Switch lowering: analyze case-value distribution
  - If dense+contiguous and count ≥ N (threshold
    TBD, likely 4-5): emit jump table in CS
  - Else: emit linear cmp/je chain
- Jump table: subtract base, bounds check, shl bx
  1, `cs: jmp [bx + table_offset]`.
- Char switch: cbw before cmp (sign-extend to int).
- Static array initializers: place in `_DATA`
  with values at FIXUPP-resolved offsets.

## Switch fallthrough = same target; no-default = direct `jmp end`; sparse cases linear too

Fixtures `1895` (case fallthrough), `1896` (no
default), and `1897` (sparse cases) complete the
switch picture.

- `1895` (**case fallthrough = same target**):
  ```c
  case 1:
  case 2:
    r = 10; break;
  ```
  Both `case 1` and `case 2` get `je L_case1_2`
  pointing at the **same body**. No code
  duplication; just multiple labels for one body.
  Most efficient possible representation of
  fallthrough.
- `1896` (**switch without default = jmp end**):
  after all case tests:
  ```
  cmp ax, 1 / je L_case1
  cmp ax, 2 / je L_case2
  jmp L_end          ; no default: skip directly to end
  ```
  No default body, just skip past.
- `1897` (**sparse cases use linear chain too**):
  `case 10: ... case 100: ... case 1000:` emits
  the same cmp/je sequence regardless of value
  spread. Case constants encoded as imm16 (e.g.,
  `3d e8 03` for cmp ax, 1000). No jump-table
  specialization even for large gaps.

So **all switch shapes use linear cmp/je chain**:
- Fallthrough: shared body via multiple jcc → same target
- No default: skip past via `jmp end`
- Sparse / dense / few / many cases: all linear

This is consistent with BCC's "compile each case
independently" approach. No advanced
specialization. Per-case overhead is constant:
~5 bytes (3 cmp + 2 je) regardless of value
density.

For the Rust reimplementation:
- All switches lower to linear cmp/je chains.
- Fallthrough: emit single body, multiple labels
  pointing at it.
- No default: `jmp end` after the chain.
- Case constants always imm16 (or AX-form `3d`).

## `goto` doesn't fuse with `if`; dead code after return emitted; `switch` = linear cmp/je chain

Fixtures `1892` (goto label), `1893` (dead code
after return), and `1894` (switch statement)
finalise the control-flow catalog.

- `1892` (**`if (cond) goto X;` no fusion**): like
  `if (cond) break;`, BCC compiles as:
  ```
  cmp si, 5
  jge L_skip      ; inverse-jcc
  jmp loop        ; the goto (unconditional)
  L_skip:
  ```
  Could have been `jl loop` (one instruction) but
  BCC keeps the cmp/jcc/jmp pattern per its "each
  statement independent" rule.
- `1893` (**dead code after return is emitted**):
  ```c
  return x;       // first return
  x = 99;         // dead
  return x + 1;   // dead
  ```
  compiles to:
  ```
  mov ax, si
  jmp epilogue     ; first return
  ; DEAD CODE follows:
  mov si, 99
  mov ax, si / inc ax
  epilogue:
  pop si / pop bp / ret
  ```
  BCC emits the dead code into the OBJ. No DCE.
  Confirms: **BCC 2.0 performs zero optimizations**
  beyond syntactic constant folding.
- `1894` (**switch = linear cmp/je chain**): no
  jump table; each case becomes a cmp+je pair:
  ```
  mov ax, [x]
  cmp ax, 1 / je L_case1
  cmp ax, 2 / je L_case2
  cmp ax, 3 / je L_case3
  jmp L_default
  L_case1: ...; jmp L_end
  L_case2: ...; jmp L_end
  L_case3: ...; jmp L_end
  L_default: ...; jmp L_end
  L_end:
  ```
  Per-case cost: 5 bytes (3 cmp + 2 je). The
  `break` after each case becomes `jmp L_end`.
  Default goes to its own block. All case bodies
  share a single L_end target.
  
  This is simple but suboptimal for dense
  enum-like switches. A jump table (`jmp [table +
  case*2]`) would be O(1) and smaller for >4
  cases, but BCC always uses the linear chain.

So **BCC's compilation philosophy** is:
1. Compile each statement independently — no
   peephole fusion with surrounding context
2. No dead-code elimination — emit what the source
   specifies, including unreachable code
3. No control-flow optimisations — switches are
   linear chains, not jump tables; if-break/if-
   goto get inverse-condition compilation
4. Only syntactic constant folding (compile-time
   constants in expressions); no algebraic
   simplification, no CSE, no DCE

This makes BCC's codegen **highly predictable** —
each source statement maps to a small, fixed
sequence of instructions. Easy to byte-exact
reproduce.

## `near` overrides model default; mixed near/far in same TU; dense switch always CS-relative

Fixtures `1748` (near ptr in -ml), `1749` (near fn
in -ml), and `1750` (dense switch in -ml) show how
explicit `near`/`far` qualifiers interact with the
memory model defaults.

- `1748` (**`near` ptr in -ml**): an explicit
  `int near *p` in large model produces a **2-byte
  pointer** with `mov ax, [si]` direct deref —
  same as the small-model default. The `near`
  qualifier **overrides** the model's far-data
  default. Useful for ptr-to-stack (which uses SS
  anyway and doesn't need a far ptr).
- `1749` (**`near` fn in -ml**): a `int near
  helper(int x)` in large model gets:
  - **near return** (`5d c3`) instead of `5d cb`
  - **args at `[bp+4]`** (not `[bp+6]`) because the
    return address is now near (2 bytes not 4)
  - **caller emits plain `call near`** (`e8`)
    without `push cs`
  
  So a single TU can have **mixed near and far
  functions**. The compiler tracks each function's
  ABI based on its declaration and emits the
  correct call/ret pair. main is far (model default)
  while helper is near.
- `1750` (**dense switch in -ml**): the indexed-
  table dispatch uses **`cs:[bx + offset]`** (`2e
  ff /4`) — **identical to small model**. CS-
  relative addressing doesn't depend on the data
  model since the jump table lives in the code
  segment. Both small and large models produce
  byte-identical switch dispatch sequences (modulo
  the surrounding function ABI bytes).

So the model interaction is per-function-symbol-level:

| Qualifier | Effect in -ms | Effect in -ml |
|-----------|---------------|---------------|
| (default) | near (matches model) | far (matches model) |
| `near` fn | near (same) | **near** (overrides) |
| `far` fn | **far** (overrides) | far (same) |
| (default) ptr to data | near (matches model) | far (matches model) |
| `near *` | near (same) | **near** (overrides) |
| `far *` | **far** (overrides) | far (same) |

This per-function tracking is important for the
Rust reimplementation:
- Track `ABI = near|far` per fn-decl symbol based
  on qualifier + model default.
- Emit `push cs` + `e8` for far-call sites; just
  `e8` for near-call sites.
- Emit `ret` (`c3`) or `retf` (`cb`) in epilogue
  per function's own ABI flag.
- Param offsets: `bp+4` (near) or `bp+6` (far).

## Sparse switch search-table CS-relative; block scope reuses slots; typedef fn-ptr identical

Fixtures `1742` (sparse switch large base), `1743`
(block-scoped declarations), and `1744` (typedef
fn pointer) cover three remaining shapes.

- `1742` (**sparse switch with large base**):
  Confirmed the **search-table dispatch strategy**:
  ```
  mov ax, x
  mov [scrutinee_slot], ax
  mov cx, N_cases       ; loop count
  mov bx, &case_value_table
  loop_start:
  mov ax, cs:[bx]       ; 2e 8b 07 — read case value from CS
  cmp ax, [scrutinee]
  je dispatch           ; 74 06 — short forward
  inc bx ; inc bx       ; advance 2 bytes
  loop loop_start       ; e2 f4 — dec cx, jump if non-zero
  jmp default
  dispatch:
  jmp word ptr cs:[bx + 2*N_cases]
  ```
  Key insights:
  - **`2e` CS-override prefix** is used for table reads — case values and offsets are stored in code segment (right after the dispatch code).
  - **`loop`** instruction (`e2 rel8`) drives the iteration — single instruction handles dec+jcc.
  - **Two parallel tables**: N case values followed by N target offsets, indexed via `[bx + 2*N]` adjustment.
  - Used when N ≥ 4 and cases are sparse (non-dense).

  This is distinct from the indexed-table strategy (dense cases) which uses `(scrutinee - base) * 2` as direct table index without a search loop.
- `1743` (**block scope reuses stack slots**): a
  nested `{ }` block's locals can reuse slots that
  earlier (now-out-of-scope) locals occupied:
  ```
  Block 1: a at [bp-2], b at [bp-4]
  Block 2: c at [bp-2]    ← reuses a's slot!
  ```
  Total frame is only 4 bytes (2 + 2 = 4 for the
  max in-scope at any point) instead of 6 (a + b
  + c). BCC tracks variable lifetimes via lexical
  scope and recycles slots.
- `1744` (**typedef fn-ptr identical to direct**):
  `typedef int (*op_t)(int); op_t f = dbl; f(7)`
  produces **byte-identical** code to `int (*f)(int)
  = dbl; f(7)`. `typedef` for function-pointer
  types is purely syntactic. Indirect call uses
  `ff 56 disp` (`ff /2` call near r/m16 with
  bp-relative address).

For the Rust reimplementation:
- Track variable lifetimes during AST scope
  analysis; assign stack offsets after a "max
  concurrent live set" pass to enable slot reuse.
- Switch dispatch strategy selection:
  - 1-3 cases → linear `cmp/je` chain
  - 4+ cases, dense → indexed table
  - 4+ cases, sparse → search-table with `loop` +
    `cs:[bx]` reads
- typedef fully resolved at parse time, never
  reaches codegen.

## Large frame uses `sub sp, imm16` + disp16; switch-default-only no dispatch; EXTDEF for extern

Fixtures `1739` (200-byte stack frame), `1740`
(switch with only default), and `1741` (extern fn
decl) close out the call/frame/dispatch picture.

- `1739` (**large stack frame**): a 200-byte local
  array forces use of **`sub sp, 200`** via the
  `81 /5 imm16` form (4 bytes total: `81 ec c8
  00`). Then `mov [bp-200], imm` uses **disp16
  addressing** (mod=10, `c7 86 38 ff 07 00`) — the
  offset `0xff38` = -200 in two's complement
  doesn't fit imm8.

  **Stack frame allocation tiers**:
  | Size N | Encoding | Bytes |
  |--------|----------|-------|
  | N=1 | `dec sp` (`4c`) | 1 |
  | N=2 | `dec sp ; dec sp` | 2 |
  | 3 ≤ N ≤ 127 | `sub sp, imm8` (`83 ec imm8`) | 3 |
  | N > 127 | `sub sp, imm16` (`81 ec imm16`) | 4 |

  **Stack frame addressing tiers** for `[bp+disp]`:
  | |disp| | Encoding (mod) | Bytes |
  |--------|--------------|-------|
  | 0 | mod=00 (rare) | (no disp) |
  | ≤ 127 | mod=01 disp8 | 1 extra |
  | > 127 | mod=10 disp16 | 2 extra |

  So `mov [bp+disp8], imm16` = 4 bytes, `mov
  [bp+disp16], imm16` = 5 bytes. Same instruction,
  larger displacement field.
- `1740` (**switch with default-only**): emits **no
  dispatch table** — just `jmp short 0` (`eb 00`)
  followed by the default body. Since there are
  no case labels, no scrutinee comparison is needed
  — execution always falls into default. The
  `eb 00` is a 2-byte break-target placeholder for
  the switch's end label.
- `1741` (**`extern` declared, not defined**): only
  emits an **EXTDEF record** for the function. The
  call site uses **`e8 imm16`** (near relative call)
  with `0x0000` placeholder; the linker resolves
  via FIXUPP at link time. No code body emitted for
  the external. Confirms symbol-linkage categories
  ([[batch-463-static-fn]]).

Final symbol-linkage / emit categories:
| Category | OBJ output |
|----------|------------|
| `extern` (default) fn, defined | PUBDEF + emit + EXTDEF (for callers) |
| `static` fn, defined | emit only (no PUBDEF) |
| `extern` fn, declared not defined | EXTDEF only |
| Local automatic | (no symbol output) |
| `extern` global, defined | PUBDEF + LEDATA/BSS |
| `static` global, defined | LEDATA/BSS only |
| `extern` global, declared only | EXTDEF only |

## Switch on `char` (cbw + table); default-only (no dispatch); reorder

Fixtures `1607` (switch on `char` scrutinee with 4
dense cases), `1608` (switch with only a default
clause), and `1609` (4 dense cases in scrambled
source order: `3, 1, 2, 0`) all pass on the first
capture.

- `1607`: char scrutinee triggers **byte-load + `cbw`
  promotion** before the standard jump-table
  dispatch: `mov al, [bp-1] / cbw / mov bx, ax / cmp
  bx, 3 / ja default / shl bx, 1 / jmp cs:[bx +
  table]`. The promotion is essentially zero-cost.
  Negative chars (with sign-extended high byte set)
  are correctly treated as out-of-range by the
  unsigned `ja` bounds check.
- `1608`: a switch containing **only `default:`** has
  **no dispatch at all** — the scrutinee is
  evaluated (stored to its slot if it has side
  effects) but never tested. The default body runs
  unconditionally. Two `eb 00` no-op jumps remain
  as artifacts of the loop/dispatch template
  (one between scrutinee setup and body, one after
  body before end label) — consistent with BCC's
  "always emit template skeleton" style.
- `1609` (**important**): cases declared out of
  source order (e.g. `case 3, case 1, case 2, case
  0`) produce **case bodies in source order** but
  **jump-table entries sorted by case value**. The
  table indexed by value `i` always points at the
  body for `case i`, regardless of which position
  it appears in the source. So:
  - Body layout: source order
  - Table layout: sorted by case value
  This means the encoder must sort cases by value
  when generating the table, while emitting bodies
  in source order with forward `jmp` to the
  end_switch label.

Updated final switch dispatch rules:
- ≤ 3 cases: linear chain (tested in source order)
- ≥ 4 dense: indexed jump-table (sorted by value,
  bodies in source order)
- ≥ 4 sparse: linear-search CS-table (sorted by
  value)
- char scrutinee: prefix `cbw` to promote to int
  before any of the above
- only `default`: no dispatch, body runs
  unconditionally

## Switch dispatch: 3 strategies — linear, indexed-table, search-table

Fixtures `1604` (4 sparse cases), `1605` (4 dense
cases with non-zero base), and `1606` (3 dense
cases with default) all pass on the first capture
and complete the switch-dispatch classification:

**Three distinct dispatch strategies:**

1. **Linear cmp-jcc chain** (≤ 3 cases): each case
   tested in turn with `cmp ax, value / je
   case_body`. Default falls through. (Fixtures
   `1598`, `1599`, `1600`, `1606`.)

2. **Indexed jump-table** (≥ 4 *dense* cases):
   - For 0-based dense (`case 0; case 1; ...`):
     `cmp bx, max / ja default / shl bx, 1 / jmp
     cs:[bx + table]`.
   - For non-zero-base dense (`case 5; case 6;
     ...`): identical but with a prefixing `sub
     bx, base` to normalise the index to 0..N-1.
   - Table holds N word-sized target offsets.
3. **Linear-search CS-table** (≥ 4 *sparse* cases —
   `1604`): novel third strategy! BCC emits a
   linear-search loop using the 8086 `LOOP`
   instruction:
   ```
   mov [bp-4], scrutinee   ; save to stack
   mov cx, N               ; number of cases
   mov bx, table_offset    ; CS-table base
   search:
     mov ax, cs:[bx]       ; read case value
     cmp ax, [bp-4]        ; compare to scrutinee
     je found
     inc bx; inc bx        ; advance by 2
     loop search           ; LOOP: dec cx, jnz
   jmp default
   found:
     jmp cs:[bx + 2*N]     ; jump through paired
                           ; target offset
   ```
   The CS-table stores the N case values
   followed by N target offsets. The `jmp cs:
   [bx + 2*N]` indexes 2*N bytes past the
   matched value to find its corresponding target.

So the lowering decision is:
- ≤ 3 cases: linear chain
- ≥ 4 dense consecutive: indexed jump-table (with
  optional base subtract)
- ≥ 4 not-dense: linear-search CS-table with `LOOP`

The sparse strategy trades O(N) lookup time for
compact table size (no gaps).

## Switch dispatch cutoff: **4 dense cases triggers jump-table**

Fixtures `1601` (4 dense), `1602` (5 dense), and
`1603` (6 dense) all use a **jump-table dispatch**,
while `1598` (2 cases) and `1600` (3-4 with sparse
intent) and `132/072` (3 dense cases) all use linear
cmp-jcc. The cutoff: **4 or more consecutive dense
cases** triggers BCC's jump-table dispatch.

Jump-table dispatch shape (from `1601`):
```
8b 5e fe       mov bx, [bp-2]      ; scrutinee → BX
83 fb 03       cmp bx, 3           ; compare to max case
77 1b          ja default          ; unsigned above → default/end
d1 e3          shl bx, 1           ; scale by 2 (word offsets)
2e ff a7 d16   jmp cs:[bx + table] ; indirect jmp through CS-prefixed
                                   ; table base address
```
Followed by case bodies, with a 2-byte-per-entry
table in the code segment containing the offset of
each case's body label (relative to CS).

Notable details:
- **Bounds check is unsigned** (`ja`, opcode `0x77`)
  — `case 0` is always covered, so anything `<0`
  (signed-wise) is wraps to "above 3" in unsigned
  terms and goes to default. So negative scrutinees
  also go to default.
- **`shl bx, 1`** scales the index since table
  entries are 2 bytes (word offsets).
- **`2e` prefix** (CS segment override) — the jump
  table lives in the code segment (`_TEXT`), so the
  indirect `jmp [bx + disp]` reads from CS.
- The table itself has **2 bytes per case** + 0
  bytes for default (default just falls through the
  initial bounds check).

So the lowering rule:
- Dense 0..N-1 cases with N ≤ 3: linear cmp-jcc chain
- Dense 0..N-1 cases with N ≥ 4: jump-table via
  `cmp / ja / shl / jmp cs:[bx + table]`

Sparse case sets (e.g. `case 1; case 5; case 10`)
likely use linear chain regardless of count — needs
a sparse-case probe to confirm.

## Small switch: linear cmp-jcc chain; `case 0` uses `or ax,ax` shortcut

Fixtures `1598` (2 cases no default), `1599` (1
case + default), and `1600` (4-case switch on array
element) all pass on the first capture.

- `1598`: small switch (2-3 cases) lowers as a
  **linear cmp-jcc chain**, not a jump table. The
  scrutinee is loaded into AX, then each case is
  tested in order with `cmp / je case_body`. After
  all tests fail, an unconditional `jmp end` (with
  no `default` clause) or `jmp default_body` takes
  the fallthrough.
- **`case 0` special-cased**: `or ax, ax / je`
  (2-byte test) is used for the zero case instead of
  the longer `cmp ax, 0 / je`. So switch-on-zero
  gets the same 2-byte truthiness check as
  `if (x)`.
- Other case values use **`cmp ax, imm16`** via the
  AX-specific `0x3D` short opcode (3 bytes total,
  even for imm fitting in imm8 sign-ext range). BCC
  canonicalises on `0x3D` for AX-with-imm cmp,
  matching the AX-with-imm `add`/`sub`/`or` family
  policy from [[batch-400-imm8-policy]].
- Each case body ends with `jmp end_switch`
  (joining all paths to a single end label). The
  case bodies are laid out in source order *after*
  the dispatch chain, with the end label after
  them.
- `1600`: confirms the same pattern for 4 cases +
  default. Each cmp-jcc against {1, 2, 3} in order;
  fallthrough goes to default. Body labels are
  forward jumps from the dispatch.

For larger switches (e.g. `072-switch-many-dense`
not re-probed here), BCC uses a different jump-
table strategy. The cutover between linear-chain
and jump-table likely correlates with case
density/count — needs a dedicated probe to
characterise.

## `switch (n % 3)`, struct with int-array field, `a + 'A'`

Fixtures `1448` (`int classify(int n) { switch (n %
3) { ... } } return classify(7);` — switch dispatching
on a modulo expression), `1449` (`struct S { int v[3];
}; struct S s = {{1,2,3}}; return s.v[0] + s.v[2];` —
struct whose only field is an int array, with brace-
nested init), and `1450` (`int a = 5; return a + 'A';`
— int sum with a char literal RHS) all pass on the
first capture. `1448` confirms switch on expression:
`n % 3` computes into AX first (via idiv), then the
dense small-switch dispatch uses AX. 7%3=1 → 200.
`1449` confirms struct-with-int-array layout: the
struct takes the same space as the bare `int v[3]`
(6 bytes), and `s.v[0]` etc. compute offsets through
the struct first then the array. Sum 1+3 = 4. `1450`
confirms char literal in int arith: `'A'` folds to 65
at parse time, so we see `mov ax,[bp-a] / add ax,65`.
Result 70.

## `a ^= 0xff`, switch-in-fn with returns, `f() ? :`

Fixtures `1280` (`int a=0x55; a ^= 0xff; return a;` —
int compound XOR with a mask), `1281` (`int classify
(int x) { switch (x) { case 0: return 100; case 1:
return 200; default: return 0; } }` — switch with
explicit `return` from each case rather than `break`-
to-join), and `1282` (`int f(void) { return 1; }
return f() ? 10 : 20;` — function call result used as
the ternary condition) all pass on the first capture.
`1280` confirms `^=` with const folds into a single
`xor word ptr [bp-N],0FFh` directly. `1281` is the
case-returns variant of switch: each case ends with
`return` rather than `break`, so no shared join point
is reached -- BCC emits direct jumps to the function
epilogue. `1282` confirms call-as-cond: AX is loaded
from the call return, tested with `or ax,ax / jne`,
then either branch runs through the standard ternary
materialization to the return epilogue.


## Sparse switch — search-table strategy (fixture `2339`)

Four sparse case labels (`1`, `5`, `10`, `100`) — too sparse for a
dense indexed jump table, but ≥4 cases is enough that BCC declines
the linear `cmp/je` chain. Instead it emits a **search-table**: a
short loop over an inline value table, followed by an indirect jump
through a parallel offset table.

```
mov  cx, 4              ; b9 04 00      ← case count
mov  bx, offset table   ; bb 49 00      ← FIXUPP'd to inline data
cs:mov ax, [bx]         ; 2e 8b 07      ← read next case value
cmp  ax, [bp-4]         ; 3b 46 fc      ← vs scrutinee
je   found              ; 74 06
inc  bx / inc  bx       ; 43 43          ← step 2 bytes
loop top                ; e2 f4
jmp  default            ; eb 18
found:
cs:jmp [bx + 8]         ; 2e ff 67 08    ← indirect jmp via offset table
```

The data table is emitted in `_TEXT` right after the function body:

```
01 00 05 00 0a 00 64 00   ; case values: 1, 5, 10, 100 (word each)
2c 00 31 00 36 00 3b 00   ; case body offsets (word each)
```

So **BCC has three switch dispatch strategies**:

| Strategy | Trigger | Codegen |
|---|---|---|
| Linear chain | small N (≤3) | per-case `cmp ax, K / je body` |
| Search table | ≥4 sparse cases | value+offset tables in CS, `loop` scan + indirect `jmp [bx+disp]` |
| Indexed (dense) jump | ≥8 contiguous cases | `shl bx, 1 / 2e ff a7 disp16` direct CS-indirect jmp |

Threshold between linear and search-table is N=4 (4 cases here triggered
search-table). Threshold between search-table and dense is density: the
2337 fixture had 10 contiguous cases and chose dense; this one has 4
non-contiguous cases (range 1..100) and chose search. Density >50% of
the range may be the rule.

## Dense jump table — threshold revised (fixture `2359`)

Earlier finding from fixture `2337` set the dense-jump-table threshold
at ≥8 contiguous cases. Fixture `2359` refines this: BCC chose the
dense indexed strategy for **just 4 cases** (`case 1: case 2: case 3:`
sharing a body, plus a separate `case 4:`). So the threshold is
**≥4 cases when fully contiguous**.

Fall-through (`case 1: case 2: case 3:` with no `break` between them)
materializes as **identical jump-table entries** pointing to the same
body offset:

```
4b              ; dec bx           ← normalize: case base = 1, so bx = x - 1
83 fb 03        ; cmp bx, 3        ← (4 cases - 1)
77 11           ; ja default
d1 e3           ; shl bx, 1
2e ff a7 30 00  ; jmp cs:[bx + 0x30]

; jump table (4 entries):
1d 00 1d 00 1d 00 22 00   ; case1, case2, case3 → 0x1d; case4 → 0x22
```

The first 3 entries are duplicates. So `case 1: case 2: case 3:` with
no break is implemented entirely at jump-table level — no extra code
in the body for fall-through, just 3 duplicate table entries pointing
at the same code.

The `dec bx` (`4b`, single byte) is the optimal form of "subtract base
1" when the case range starts at 1. Larger bases use `83 eb K` (sub
bx, K imm8) or `81 eb K` (sub bx, K imm16). Confirms case-base
normalization is independent of the table itself.

**Revised switch dispatch table**:

| Strategy | Trigger | Codegen |
|---|---|---|
| Linear chain | ≤3 cases | per-case `cmp ax, K / je body` |
| Search table | ≥4 sparse cases (gaps too wide for dense) | value+offset tables in CS, `loop` scan + indirect `jmp [bx+disp]` |
| **Dense jump table** | **≥4 contiguous cases** | normalize base (`dec bx` / `sub bx, K`); `shl bx, 1`; `2e ff a7 disp16` direct CS-indirect jmp |

## Default-only switch — dispatch elided (fixture `2370`)

`switch (x) { default: r = 99; break; }` (no explicit cases) skips
the dispatch entirely. The scrutinee is still loaded but no `cmp`
sequence follows — the default body runs unconditionally.

```
8b 46 fe                ; mov ax, [x]   ← scrutinee load (still happens)
                        ; (no cmp/je sequence — no explicit cases)
eb 00                   ; jmp +0        ← no-op stand-in
be 63 00                ; mov si, 99    ← default body
eb 00                   ; jmp end (break)
```

The `eb 00` is the residual jump that any switch generates between
dispatch and body block — here it's a no-op since dispatch produced
nothing. Confirms BCC's switch compiler doesn't special-case the
zero-case form, just emits an empty dispatch.

## `default:` source position vs. dispatch order (fixture `2374`)

Placing `default:` BEFORE the explicit cases:

```c
switch (x) {
  default: r = 99; break;
  case 1: r = 11; break;
  case 2: r = 22; break;
}
```

BCC dispatches like this:

```
; Dispatch (in case-source order excluding default):
3d 01 00 / 74 0c        ; cmp ax, 1 / je case_1
3d 02 00 / 74 0c        ; cmp ax, 2 / je case_2
eb 00                   ; (jmp +0 — fall through to default body)
; default body (first in code, matching source order):
be 63 00 / eb 0a        ; mov si, 99; jmp end (skip explicit cases)
; case_1 body:
be 0b 00 / eb 05        ; mov si, 11; jmp end
; case_2 body:
be 16 00 / eb 00        ; mov si, 22; jmp end
; end
```

So **`default:`'s source position affects CODE LAYOUT** (the
default body is emitted first since it appeared first in source)
**but NOT DISPATCH ORDER** (all explicit `case` comparisons happen
first, default is the fallthrough). After the dispatch's last
`cmp/je` falls through, the default body runs; each explicit case
then has its own `jmp end` to skip over any subsequent bodies.

This proves BCC separates two concerns: case dispatch order is
fixed (explicit cases first, default last) while body emission
follows source order. The skip-jumps between bodies handle
correctness regardless of whether default lives in the middle or
the top of the case bodies.

## `case 0:` — zero-test peephole (`or ax, ax`) (fixture `2384`)

When a switch case label is exactly `0` and the dispatch is the
linear chain, BCC swaps the `cmp ax, 0 / je body` (5 bytes) for the
shorter `or ax, ax / je body` (4 bytes) — the same zero-test peephole
used by `if (x == 0)`.

```c
enum Color { RED, GREEN, BLUE };  // RED = 0
switch (c) {
  case RED: ...        // case 0
  case GREEN: ...      // case 1
  case BLUE: ...       // case 2
}
```

```
8b 46 fe                ; mov ax, c
0b c0                   ; or ax, ax           ← case 0 — zero-test peephole
74 0c                   ; je RED_body
3d 01 00                ; cmp ax, 1           ← case 1 — standard cmp form
74 0c                   ; je GREEN_body
3d 02 00                ; cmp ax, 2
74 0c                   ; je BLUE_body
```

Saves 1 byte per `case 0:` arm. Most useful when enum values start
at zero (the C90 default), which they do in every enum unless an
explicit value reassigns. So most switch-on-enum dispatches benefit
from this when the chain form is selected (≤3 cases).

For the search-table or dense-jump-table strategies, the zero-test
peephole doesn't apply — the dispatch doesn't compare individual
case values.

Confirms enum constants fold to their integer values at parse time
(RED=0, GREEN=1, BLUE=2), and case-label arithmetic peepholes (like
zero-test) apply uniformly to any compile-time-constant label
expression.

## Single-case switch — degenerates to `cmp+je`, NO dispatch table

Fixture `2542-switch-single-case-obj`:

```c
int x;
x = 1;
switch (x) {
  case 1: return 10;
}
return 0;
```

```
55 8b ec 4c 4c                 prologue + 2B local
c7 46 fe 01 00                 x = 1
8b 46 fe                       mov ax, x
3d 01 00                       cmp ax, 1            ; AX-accumulator cmp
74 02                          je +2 → case-1
eb 05                          jmp +5 → after-switch
                               ; case 1:
b8 0a 00                       mov ax, 10
eb 04                          jmp +4 → epi
                               ; after-switch / default:
33 c0                          xor ax, ax
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Single-case switch is **identical in cost to an if-equivalent**.
  No dispatch table or search-table — just `cmp+je <case-body>;
  jmp <after>`. So the jump-table threshold is **at least 2 cases**
  (and from earlier findings, the dense table starts at 4+ contiguous
  cases).
- The `cmp ax, imm16` uses the **AX-accumulator form** (`3d` opcode,
  3 bytes total). Same length as the generic `83 f8 imm8` for fits-
  in-imm8, but BCC picked the AX form regardless of immediate width.
- The case body ends with `jmp epi` — switch doesn't fall through
  to the (implicit) default; the case explicitly `return`'s.
- The "after-switch" code (`xor ax, ax; jmp epi`) IS the post-switch
  `return 0` from the source. No invisible "switch end" label or
  bookkeeping — the source's structure is preserved 1:1.


## 4 contiguous cases — dispatches via `jmp word ptr CS:[bx+disp16]` table

Fixture `2550-switch-four-contig-obj`:

```c
int x = 2;
switch (x) {
  case 0: return 10;
  case 1: return 20;
  case 2: return 30;
  case 3: return 40;
}
return 99;
```

```
55 8b ec 4c 4c                 prologue + 2B local
c7 46 fe 02 00                 x = 2
8b 5e fe                       mov bx, x
83 fb 03                       cmp bx, 3
77 1b                          ja +27 → DEFAULT       ; UNSIGNED range check
d1 e3                          shl bx, 1              ; bx *= 2 (entry size)
2e ff a7 36 00                 jmp word ptr CS:[bx+0x0036]
                               ; --- case 0 body ---
b8 0a 00                       mov ax, 10
eb 14                          jmp → EPI
                               ; --- case 1 body ---
b8 14 00                       mov ax, 20
eb 0f                          jmp → EPI
                               ; --- case 2 body ---
b8 1e 00                       mov ax, 30
eb 0a                          jmp → EPI
                               ; --- case 3 body ---
b8 28 00                       mov ax, 40
eb 05                          jmp → EPI
                               ; --- default ---
b8 63 00                       mov ax, 99
eb 00 8b e5 5d c3              epilogue
                               ; --- jump table (in _TEXT, 8 bytes) ---
19 00 1e 00 23 00 28 00        ; 4 entries, each FIXUPP'd to case offset
```

Findings:
- **Dense jump-table threshold = 4 contiguous cases**. (Earlier
  fixtures showed: 1 case = if-equivalent, 2-3 still to probe but
  expected linear; 4+ contiguous → table.)
- **Range check uses `ja` (unsigned)**: `cmp bx, MAX; ja DEFAULT`.
  Treats the switch expression as if it could be any 16-bit value —
  negative values map to "above MAX" via unsigned interpretation,
  routing to default. So even with signed `int x`, the dispatch is
  done unsigned-style.
- **Dispatch instruction = `2e ff a7 disp16`** (5 bytes):
  `jmp word ptr CS:[bx+disp16]`. The `2e` segment override is
  essential — the jump table lives in the code segment.
- **Table entries are 2-byte word offsets** into the code segment,
  each carrying a FIXUPP to relocate to the case body's address.
- The table itself is laid out **after the default body**, at the
  end of the function's code. The "disp16" in the dispatch points
  to the table's start.
- All bodies (case AND default) end with `jmp <EPI>` for merge.


## Sparse switch (4+ non-contiguous cases) — **search table**, NOT jump table

Fixture `2567-switch-sparse-search-obj`:

```c
int x = 10;
switch (x) {
  case 1:   return 11;
  case 10:  return 22;
  case 100: return 33;
  case 500: return 44;
}
return 0;
```

Key main body sections:
```
83 ec 04                       sub sp, 4              ; 4B = x + temp
c7 46 fe 0a 00                 x = 10
8b 46 fe                       mov ax, x
89 46 fc                       mov [bp-4], ax         ; spill x to compare slot
b9 04 00                       mov cx, 4              ; case count
bb 45 00                       mov bx, &case-values  ; (FIXUPP)
                               ; ---- LINEAR SEARCH LOOP ----
2e 8b 07                       mov ax, CS:[bx]        ; next case value
3b 46 fc                       cmp ax, [bp-4]
74 06                          je → MATCH
43 43                          inc bx; inc bx         ; advance pointer
e2 f4                          loop -12 → LOOP TOP
eb 18                          jmp → DEFAULT          ; no match
                               ; ---- MATCH ----
2e ff 67 08                    jmp word ptr CS:[bx+8] ; skip case-values (4×2B), index handler offset
                               ; ---- case bodies (mov ax,K; jmp epi) ----
                               ; ---- default body (xor ax,ax) ----
                               ; ---- search tables (in _TEXT) ----
01 00 0a 00 64 00 f4 01         ; CASE VALUES: 1, 10, 100, 500
29 00 2e 00 33 00 38 00         ; HANDLER OFFSETS: 4 FIXUPP'd code offsets
```

Findings:
- **Sparse switch (4+ non-contiguous cases) uses a LINEAR SEARCH
  TABLE**, not the dense jump-table from `2550`.
- Two parallel tables in `_TEXT`:
  - **CASE VALUES**: N × 2-byte case constants
  - **HANDLER OFFSETS**: N × 2-byte FIXUPP'd code offsets to bodies
- Dispatch uses **`loop` instruction** (`e2 disp8`) — decrements CX
  and branches if non-zero. This implies a 256-case maximum because
  `loop` uses disp8.
- On match, the offset between the matched case-value and the
  corresponding handler is exactly `+8` bytes = `(case-count - matched-index) × 2 + matched-index × 2 = case-count × 2`. So `jmp word ptr CS:[bx + case_count*2]` retrieves the handler.
- Switch strategy thresholds (now complete):

| case count | layout      | dispatch                            |
|------------|-------------|-------------------------------------|
| 1          | linear      | `cmp; je body; jmp default`         |
| 2-3        | linear chain (to probe) | sequential cmp/je       |
| 4+ contig  | dense table | `cmp; ja default; shl; jmp CS:[bx+]` |
| 4+ sparse  | search table | `loop` linear scan, dual tables    |


## 2-case switch — LINEAR CHAIN (cmp+je per case)

Fixture `2573-switch-two-cases-obj`:

```c
switch (x) {
  case 1: return 10;
  case 2: return 20;
}
return 99;
```

```
8b 46 fe                       mov ax, x
3d 01 00                       cmp ax, 1
74 07                          je → case 1 body
3d 02 00                       cmp ax, 2
74 07                          je → case 2 body
eb 0a                          jmp → default
                               ; bodies + default follow
```

Findings:
- 2-case switch uses **sequential cmp+je per case**, then `jmp default`.
- Uses `3d imm16` (AX-accumulator cmp form, 3 bytes) for each case.

## 3-case switch — STILL linear chain (even sparse)

Fixture `2574-switch-three-cases-obj` (cases 1, 2, 5):

```
8b 46 fe                       mov ax, x
3d 01 00 74 0c                 cmp+je case 1
3d 02 00 74 0c                 cmp+je case 2
3d 05 00 74 0c                 cmp+je case 5
eb 0f                          jmp default
```

Findings:
- 3-case switch ALSO uses linear chain — even when cases are non-
  contiguous (1, 2, 5). The table dispatch (dense or search) does
  not kick in below 4 cases.
- **Complete switch threshold table**:

| N cases  | layout             |
|----------|--------------------|
| 1        | if-equiv (cmp+je+jmp default) |
| 2        | linear chain (2× cmp+je) |
| 3        | linear chain (3× cmp+je) — confirmed sparse too |
| 4+ contig| dense table        |
| 4+ sparse| search table       |

  Per-case cost in linear chain: 5 bytes (cmp 3B + je 2B).
  At N=4 the table-dispatch overhead (range check + indirect jmp)
  starts paying off — dense table is ~12B header + 2B/entry,
  search table is ~14B header + 4B/entry.


## Switch case fallthrough — NO jmp between cases (default C semantic)

Fixture `2581-switch-fallthrough-obj`:

```c
switch (x) {
  case 1:
    r = r + 1;
    /* FALLTHROUGH (no break) */
  case 2:
    r = r + 10;
    break;
  case 3:
    r = r + 100;
}
```

Main body (cases section):
```
                               ; --- case 1 body ---
8b c6 40 8b f0                 r = r + 1
                               ; (NO jmp here — falls through!)
                               ; --- case 2 body ---
8b c6 05 0a 00 8b f0           r = r + 10
eb 07                          jmp → after-switch  (the BREAK)
                               ; --- case 3 body ---
8b c6 05 64 00 8b f0           r = r + 100
                               ; (falls through to after-switch)
                               ; --- after-switch ---
8b c6                          mov ax, r
```

Findings:
- **Case fallthrough is the default**: BCC inserts NO instruction
  between case bodies. Case 1's last instruction is followed
  immediately by case 2's first.
- An explicit `break` compiles to `jmp <after-switch>`.
- The last case (case 3) here also has no break, so it just falls
  off the end of the switch (= reach the after-switch label).
- Note: this 3-case switch (cases 1, 2, 3 — all contiguous!) STILL
  uses linear chain, not dense table. Confirms that **the
  threshold for dense table is strictly N ≥ 4, even when cases are
  contiguous**.
- This means BCC's switch desugars to:
  ```
  if      (x == 1) goto case_1;
  else if (x == 2) goto case_2;
  ...
  else goto after_switch;
  case_1: body1;     /* fall through */
  case_2: body2; break;
  ...
  after_switch:
  ```
  The dispatch is JUST the chain of cmp/je; each `case label:`
  becomes a goto target, and `break` is a jmp to after-switch.


## Switch with `default` in middle — dispatch unchanged, bodies stay in source order

Fixture `2600-switch-default-middle-obj`:

```c
switch (x) {
  case 1: return 10;
  default: return 99;
  case 2: return 20;
}
return 0;
```

```
55 8b ec 4c 4c                 prologue + 2B local
c7 46 fe 05 00                 x = 5
8b 46 fe                       mov ax, x
3d 01 00                       cmp ax, 1
74 07                          je → case_1
3d 02 00                       cmp ax, 2
74 0c                          je → case_2
eb 05                          jmp → default
                               ; --- case 1 body (source order) ---
b8 0a 00                       mov ax, 10
eb 0e                          jmp → epi
                               ; --- default body ---
b8 63 00                       mov ax, 99
eb 09                          jmp → epi
                               ; --- case 2 body ---
b8 14 00                       mov ax, 20
eb 04                          jmp → epi
                               ; --- after-switch ---
33 c0                          xor ax, ax
eb 00 8b e5 5d c3              epilogue
```

Findings:
- **Default position is decoupled from dispatch logic**: the
  dispatch tests only the labeled cases (1, 2), and falls through
  to `jmp default` if none match — regardless of where the default
  body sits in the source.
- **Case bodies are emitted in SOURCE order** — case 1, default,
  case 2. The default's body lives between the two case bodies.
- This means: the `jmp default` instruction in dispatch can target
  ANY position in the body region; BCC computes the displacement
  to wherever the parser placed it.
- Fallthrough behavior still applies: if default has no `break`,
  it falls into case 2's body. Here all three have `return` so no
  fallthrough is visible.
- The post-switch `return 0` lives after the case bodies and is
  reached only if all bodies fall through (none here, since each
  returns).


## Empty case fall-into-next — shared body via TWO dispatch jumps to SAME label

Fixture `2620-empty-case-fall-obj`:

```c
switch (x) {
  case 1:
  case 2:
    return 12;
  case 3:
    return 30;
}
```

```
3d 01 00 74 0c                 cmp x, 1; je → BODY_12
3d 02 00 74 07                 cmp x, 2; je → BODY_12  (SAME TARGET as case 1!)
3d 03 00 74 07                 cmp x, 3; je → BODY_30
eb 0a                          jmp → after-switch
                               ; BODY_12 (case 1 + case 2):
b8 0c 00 eb 09                 mov ax, 12; jmp epi
                               ; BODY_30 (case 3):
b8 1e 00 eb 04                 mov ax, 30; jmp epi
                               ; after-switch:
33 c0 eb 00                    xor ax, ax (return 0)
8b e5 5d c3                    epilogue
```

Findings:
- An EMPTY case label that falls into the next case (no body
  between them) compiles to **TWO dispatch jumps targeting the
  SAME body address**. Body is emitted ONCE.
- `case 1: case 2: return 12;` → 2 cmp+je pairs both pointing to
  the same `mov ax, 12; jmp epi`. Single body.
- This is the case-label-coalescing rule: multiple label-only
  statements before a body produce N dispatch entries → 1 body.
- The disp8 values shrink along the chain:
  - case 1 → +12 (further forward)
  - case 2 → +7 (closer)
  - case 3 → +7 (different body)
- This generalizes: `case 1: case 2: case 3: body;` would produce
  3 cmp+je dispatches all to one body.


## Nested switch — independent dispatch + body block per level

Fixture `2641-nested-switch-obj`:

```c
switch (x) {
  case 1:
    switch (y) {
      case 2: return 12;
    }
    return 10;
  case 2: return 20;
}
return 0;
```

Body sketch:
```
; --- outer dispatch ---
mov ax, x
cmp ax, 1; je OUTER_CASE_1
cmp ax, 2; je OUTER_CASE_2
jmp DEFAULT

; --- OUTER_CASE_1 body (includes inner switch) ---
mov ax, y
cmp ax, 2; je INNER_CASE_2
jmp AFTER_INNER
INNER_CASE_2:
  ax = 12; jmp epi
AFTER_INNER:
  ax = 10; jmp epi

; --- OUTER_CASE_2 body ---
ax = 20; jmp epi

; --- DEFAULT ---
xor ax, ax
```

Findings:
- Nested switches are **structurally independent** — each switch
  has its own dispatch chain and body region. No shared dispatch
  state.
- The inner switch lives entirely inside its enclosing case's
  body, with its own labels for cases and post-switch.
- No special "inner switch overrides break/continue" mechanics
  needed at the codegen level — labels are scoped naturally by
  position in the byte stream.


## `switch (c)` with char expression — `cbw` promote + int-compare dispatch

Fixture `2655-switch-char-expr-obj`:

```c
int decode(char c) {
  switch (c) {
    case 'A': return 1;
    case 'B': return 2;
    case 'C': return 3;
  }
  return 0;
}
```

```
55 8b ec                       prologue
8a 46 04                       mov al, c        (byte load)
98                             cbw              (promote char → int)
3d 41 00                       cmp ax, 'A'      (3-byte AX-form)
74 0c                          je → case A
3d 42 00 74 0c                 (similar for B)
3d 43 00 74 0c                 (similar for C)
eb 0f                          jmp → default
...
```

Findings:
- `switch (c)` with `char c` (signed by default) **promotes c to
  int via `cbw`** before dispatch. Subsequent compares use the
  16-bit `cmp ax, imm16` form (3 bytes per case).
- BCC does NOT use a byte-aware optimization like `cmp al, K`
  (2 bytes). The full integer-switch shape applies uniformly.
- Each case dispatch costs 5 bytes (3 cmp + 2 je), same as int
  switches.
- The `cbw` is necessary because case labels are int-typed (`'A'`
  is `int` per C90), so a byte compare wouldn't match if the high
  byte of AX was non-zero.


## Switch with case 0 — `or ax, ax` peephole replaces `cmp ax, 0`

Fixture `2684-enum-in-switch-obj`:

```c
enum Color { RED, GREEN, BLUE };  // RED=0, GREEN=1, BLUE=2
switch (c) {
  case RED:   return 0xF00;
  case GREEN: return 0x0F0;
  case BLUE:  return 0x00F;
}
```

```
8b 46 04                       mov ax, c
0b c0                          or ax, ax           ; CASE 0 = "or"!
74 0c                          je → RED body
3d 01 00                       cmp ax, 1           ; CASE 1
74 0c                          je → GREEN body
3d 02 00                       cmp ax, 2           ; CASE 2
74 0c                          je → BLUE body
eb 0f                          jmp → default
```

Findings:
- **Case value 0 triggers a switch-dispatch peephole**: BCC emits
  `or ax, ax` (2 bytes) instead of `cmp ax, 0` (3 bytes) for the
  zero-test. Same opcode used for standalone `x == 0` tests.
- Subsequent non-zero cases use the full `cmp ax, imm16` (3B) form.
- Per-case savings: 1 byte ONLY for case 0.
- Confirms that BCC's "test against zero" peephole applies in any
  context — standalone if, switch dispatch, do-while cond, etc.
- Enum values are folded at compile time (RED → 0, GREEN → 1, etc.);
  the OBJ has no record of the enum names.


## Switch with ONLY default — `jmp default` (no dispatch)

Fixture `2720-switch-default-only-obj`:

```c
int x = 7;
switch (x) {
  default: return 99;
}
return 0;
```

```
55 8b ec 4c 4c                 prologue + 2B local
c7 46 fe 07 00                 x = 7
eb 00                          jmp → default (2B)
                               ; default body:
b8 63 00                       ax = 99
eb 04                          jmp → epi
                               ; UNREACHABLE post-switch:
33 c0                          ax = 0
eb 00 8b e5 5d c3              epilogue
```

Findings:
- **Switch with only `default:` skips dispatch entirely** — no
  load of the switch expression, no `cmp`, no test. Just `jmp` to
  the default body.
- The 2-byte `eb 00` (default-position jmp) here is the only
  remnant of the dispatch logic — and it's the standard "go to
  next instruction" jmp BCC always emits.
- This is the cleanest "always execute" case: dispatch is fully
  elided because no labeled case can match (none exist).
- The post-switch code (`return 0`) is unreachable but emitted —
  consistent with no-DCE pattern.


## Switch with NEGATIVE cases — same linear chain as positive

Fixture `2834-switch-neg-cases-obj`:

```c
switch (x) {
  case -1: return 10;
  case 0:  return 20;
  case 1:  return 30;
}
```

```
8b 46 04                       mov ax, x
3d ff ff                       cmp ax, -1 (0xFFFF)
74 0b                          je → CASE_-1
0b c0                          or ax, ax     (test == 0 peephole, 2B)
74 0c                          je → CASE_0
3d 01 00                       cmp ax, 1
74 0c                          je → CASE_1
                               ; (no default → falls through)
```

Findings:
- 3 cases use **linear chain** (not dense table). Threshold for
  table dispatch is 4+ contiguous cases.
- Negative case value `-1` uses `cmp ax, 0xFFFF` (`3d ff ff`, 3B
  imm16). No special handling vs positive.
- Comparison with 0 uses the **`or ax, ax` peephole** (`0b c0`,
  2B), saving 1 byte vs `cmp ax, 0` (3B).
- Negative + zero + positive in mixed order works fine — order is
  source-order; no sorting.


## Switch fall-through `case 1: case 2: ...` — TWO cmp-je with shared target

Fixture `2844-switch-fall-thru-obj`:

```c
switch (x) {
  case 1:
  case 2:
    return 10;
  case 3:
    return 20;
}
```

```
3d 01 00 74 0c                 cmp ax, 1; je → CASE_1_2
3d 02 00 74 07                 cmp ax, 2; je → CASE_1_2     (same target!)
3d 03 00 74 07                 cmp ax, 3; je → CASE_3
eb 0a                          jmp → AFTER_SWITCH
```

Findings:
- Fall-through case labels emit **separate compare-and-branch
  instructions** for each case, both targeting the same body label.
- 6 bytes per case-arm (3B cmp + 3B je) — fall-through adds NO
  saving at codegen level.
- BCC treats fall-through purely syntactically: `case 1: case 2:`
  is just two cases at the same address.

