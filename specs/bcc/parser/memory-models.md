# Memory models

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## In small model: `near` no-op; `far` data ptr = 4B + LES + ES override; `far` fn = `push cs / call near` + `cb`

Fixtures `2249` (near in small), `2250` (far data
ptr in small), `2251` (far fn in small) probe
explicit pointer-size keywords.

- `2249` (**`near` in small**): byte-identical to
  default. Pointer is 2 bytes, no segment
  involvement. `near` qualifier is a no-op.
- `2250` (**`far` data ptr in small**): brings
  huge-style 4-byte pointer access into small
  model:
  ```
  ; Construct p (far ptr = offset + segment):
  lea ax, [x]
  mov [p.seg], ss            ; 8c 56 fc — store SS as segment
  mov [p.off], ax
  
  ; Dereference *p:
  les bx, [p]                ; c4 5e fa — load offset+segment
  mov ax, es:[bx]            ; 26 8b 07 — with ES override (3B)
  ```
  Cost: 6 bytes for ptr (vs 2), 3-5 bytes per
  deref (vs 2-3).
- `2251` (**`far` fn in small, same TU**): the
  far fn has `cb` (retf) instead of `c3`. The
  caller within the same translation unit uses
  the **`push cs / call near`** intra-CS
  optimization (4 bytes), same as medium model's
  default:
  ```
  ; In _helper (far fn):
  ...
  5d cb              ; pop bp / retf
  
  ; Caller (same TU):
  0e                ; push cs
  e8 NN NN          ; call near (rel16)
  ```

**Pointer-size qualifier summary** (in small model):
| Qualifier | Pointer | Deref | Notes |
|-----------|---------|-------|-------|
| (default) | 2B near | `[bx]` or `[bp+d]` 2-3B | DS implicit |
| `near` | 2B near | Same as default | No-op |
| `far` | 4B (off+seg) | `les / mov es:[bx]` 5B | Explicit segment |
| `huge` | 4B + normalised | (varies) | Normalised after arith |

**Far fn call form depends on whether target is
intra-segment**:
- Same TU + intra-CS: `push cs / call near` (4B)
- Different TU (extern): full `9a` (5B with seg
  FIXUPP)

**8086 segment override prefixes**:
| Prefix | Override | Use case |
|--------|----------|----------|
| 0x26 | ES | far pointer access |
| 0x2E | CS | code reads (rare) |
| 0x36 | SS | (rare; default for [bp]) |
| 0x3E | DS | (default; usually elided) |
| 0x64 | FS | (286+ only) |
| 0x65 | GS | (286+ only) |

For BCC 2.0 8086 target, only `26` (ES) is
emitted, for far data access.

For the Rust reimplementation:
- Track per-pointer model (near/far/huge).
- Far ptr load: emit `c4 [m]` (LES).
- Far ptr deref: emit segment-override prefix
  `26` before the access.
- Far fn intra-TU: emit `push cs / call near`.

## Large model = same as medium (far code, near data via DGROUP); huge is the only model w/ far DATA

Fixtures `2213` (large fn call), `2214` (large
global int), `2215` (large global arr) clarify
what's different in large vs medium model for
basic code.

- `2213` (**large model fn call**): **byte-
  identical** to medium model — `push cs / call
  near` (`0e e8 [rel]`) intra-segment, `cb` far
  ret.
- `2214` (**large model global int**): **byte-
  identical** to medium model — near DATA access
  via DGROUP (`a1 disp16`). No segment override.
- `2215` (**large model global array**): same
  near data access pattern. No `push ds` envelope
  needed.

**Memory model differences observed** (final):
| Model | Code | Data | Code seg | Data access |
|-------|------|------|----------|-------------|
| small (-ms) | near | near | `_TEXT` | `a1 disp16` |
| compact (-mc) | near | near | `_TEXT` | `a1 disp16` |
| medium (-mm) | far | near | `<fname>_TEXT` | `a1 disp16` (DGROUP) |
| large (-ml) | far | near | `<fname>_TEXT` | `a1 disp16` (DGROUP) |
| huge (-mh) | far | **far** | `<fname>_TEXT` | `push ds / mov ds, seg / a1 / pop ds` |

So **only huge model has FAR data** in the basic
case. Compact and large in BCC 2.0 are effectively
same as small/medium respectively, as far as basic
codegen goes. Differences would only manifest with
explicit `far` data declarations or huge-data
scenarios (data > 64K).

The 5 models compress to 3 effective code-class
behaviours:
- "near code + near data": small, compact
- "far code + near data": medium, large
- "far code + far data": huge

For the Rust reimplementation:
- Treat compact = small for trivial cases; large
  = medium for trivial cases.
- Differentiating compact/large from their pairs
  requires explicit `far` data or > 64K data
  sections.

## Medium model: extern fn = full CALL FAR (`9a`); fn ptr = 4B (CS+offset); static fn = intra-seg same as global

Fixtures `2210` (medium extern fn), `2211` (medium
fn ptr), `2212` (medium static fn) characterise
medium-model function calls.

- `2210` (**medium extern fn → full CALL FAR**):
  for cross-segment extern calls, BCC emits the
  **full 5-byte CALL FAR** with FIXUPP for both
  offset and segment:
  ```
  push fmt_offset
  9a 00 00 00 00         ; CALL FAR _printf (FIXUPP for offset+segment)
  pop cx
  retf (cb)
  ```
  Contrast with intra-segment medium calls (push
  cs + call near = 4 bytes).
- `2211` (**medium fn ptr = 4 bytes**): function
  pointers in medium model are **far pointers**
  (offset + segment, 4 bytes):
  ```
  ; Construct fp:
  mov [fp.seg], cs         ; 8c 4e fe (use CS as segment)
  mov [fp.off], offset_of_dbl   ; c7 46 fc 00 00 (FIXUPP)
  
  ; Call through fp:
  ff 5e fc                 ; call far [bp-4]
                            ; ModR/M /3 = call far indirect through m32
  ```
  ModR/M `5e disp8` = /3 = call far indirect.
- `2212` (**medium static fn**): no PUBDEF, intra-
  segment `push cs / call near`. Same as global
  default fn in medium model. The `static`
  modifier only affects symbol visibility, not
  the call form.

**Medium-model call forms (complete)**:
| Call type | Form | Bytes |
|-----------|------|-------|
| Same-CS intra-segment (default or static) | `0e e8 [rel]` (push cs + call near) | 4 |
| Cross-segment extern | `9a [off] [seg]` (CALL FAR, FIXUPP) | 5 |
| Through near fn ptr (forced) | `ff /2 [m]` | varies |
| Through far fn ptr (default in medium) | `ff /3 [m]` | varies |

**Far-pointer encoding (medium model)**:
- fn pointer = 4 bytes (segment + offset, little-endian)
- Constructed: `mov [hi], cs` + `mov [lo], offset_fixup`
- Dereferenced: `ff /3` (call far indirect)

For the Rust reimplementation:
- Medium model extern: emit `9a 00 00 00 00` with
  FIXUPP for both offset and segment.
- Medium fn pointers: 4 bytes, construct via cs
  + offset FIXUPP.
- Calls through fn ptr: `ff /3` for far, `ff /2`
  for near.

## Huge model = far data + `push ds/mov ds,seg/.../pop ds`; `far *` ptr = LES BX; `near *` redundant in -ms

Fixtures `2057` (huge -mh), `2058` (explicit `far
*` in small), `2059` (explicit `near *` in small)
explore far data and the explicit far/near
keywords.

- `2057` (**huge model -mh**): segment names
  include `HELLO_TEXT`, `HELLO_DATA`, **`FAR_DATA`**
  — confirming far data. Each access loads DS:
  ```
  1e                      ; push ds (save)
  b8 [seg] [seg]          ; mov ax, segment_of_g (FIXUPP type=segment)
  8e d8                   ; mov ds, ax
  a1 [off] [off]          ; mov ax, [g] (FIXUPP for offset)
  1f                      ; pop ds (restore)
  ```
  **11 bytes** for one data access — expensive!
- `2058` (**`int far *p` in small model**): the
  far ptr is **4 bytes** (offset + segment):
  ```
  8c 5e fe                ; mov [bp-2], ds (high half = segment from DS)
  c7 46 fc 00 00          ; mov [bp-4], 0 (FIXUPP for offset, low half)
  ```
  Deref via LES:
  ```
  c4 5e fc                ; les bx, [bp-4]  (offset→bx, seg→es)
  26 8b 07                ; mov ax, ES:[bx]
  ```
  The `c4 /r` (LES) instruction is the canonical
  far-ptr load.
- `2059` (**`int near *p` in small model**):
  **byte-identical to default** (no near keyword)
  — `near` is a no-op in small model:
  ```
  be 00 00                ; mov si, 0 (FIXUPP for offset)
  8b 04                   ; mov ax, [si]
  ```

**Far/near pointer summary**:
| Type | Size | Construction | Deref |
|------|------|--------------|-------|
| `int *` (small-model default) | 2B | mov ax, offset (FIXUPP) | `8b /r` or `a1` |
| `int near *` (small) | 2B | (same as default) | (same) |
| `int far *` | 4B | mov [high], ds + mov [low], offset (FIXUPP) | `c4 /r` (les) then `26 8b /r` |
| `int *` in huge model | 4B (implicit far) | same as far | same as far |

For the Rust reimplementation:
- Far ptr type: track 4-byte representation.
- Far ptr load (construct): `mov [high], ds` (if from local DGROUP) + `mov [low], offset` with FIXUPP.
- Far ptr deref: emit LES (`c4 /r`) then segment-override prefixed mov (`26 8b /r`).
- Huge model data access: emit the full `push ds / mov ds, seg / ... / pop ds` envelope around the access.

## Medium model: intra-seg call = `push cs / call near`; arg at `[bp+6]`; retf `cb`; data still near

Fixtures `2054` (medium fn call), `2055` (medium
recursion), `2056` (medium string arg) reveal
medium model's call/return shapes.

- `2054` (**intra-segment call = `0e e8 [rel]`**):
  ```
  push 41                         ; arg
  0e                              ; push cs (1B)
  e8 ea ff                        ; call near _helper (3B)
  pop cx                           ; cleanup
  ```
  Total 4 bytes for the call + push cs, vs 5
  bytes for full CALL FAR (`9a [off] [seg]` with
  FIXUPP). Since caller and callee are in the
  **same code segment**, push cs gives the
  correct segment for the eventual `retf`.
  
  The callee returns with `5d cb` (pop bp / retf)
  which pops both offset and segment.
- `2055` (**recursive intra-seg call**): same
  `0e e8 [rel]` pattern for the recursive call.
  ```
  push (n-1)
  0e
  e8 e7 ff                        ; call near _fact (recursive)
  pop cx
  imul si                          ; * n
  ```
  Optimization applies to any intra-segment call
  in medium/large models.
- `2056` (**string arg from `_DATA`**): string
  literal still in `_DATA` (DGROUP near). Push as
  2-byte near offset with FIXUPP. Same as small
  model:
  ```
  mov ax, 0                        ; b8 00 00 (FIXUPP)
  push ax
  0e
  e8 d8 ff                        ; call near _strlen_local
  ```

**Medium-model stack frame** (and large model):
```
[bp+0]: saved BP
[bp+2]: return offset
[bp+4]: return segment           <-- extra 2 bytes
[bp+6]: first arg
```
Args start at `[bp+6]` instead of `[bp+4]` due
to the far return address.

For the Rust reimplementation:
- Medium/large code: emit `push cs / call near`
  for intra-segment calls (avoids the FIXUPP
  segment field).
- Use `retf` (`cb`) for function returns in
  medium/large.
- Arg offsets: start at `[bp+6]` in medium/large.

## Cross-model uniformity: compact/medium/large share IR-level codegen; differ only at boundaries

Fixtures `1868` (compact static local), `1869`
(medium `&&`), and `1870` (large array via ptr)
verify that **IR-level codegen patterns port
unchanged across models**; differences appear
only at function-call boundaries and pointer
representations.

- `1868` (**compact static local**): the codegen
  is **byte-identical to small-model** for the
  function body — `ff 06 disp16` (inc word [n])
  and `a1 disp16` (mov ax, [n]) via direct DS-
  relative addressing. Compact's "far data" only
  matters when explicit pointers cross segments.
  Function still uses near `ret` (`c3`) since
  compact has near code.
- `1869` (**medium `&&`**): codegen for `if (a &&
  b)` is **byte-identical to small-model** through
  the cmp/jcc chain and bool template. The ONLY
  difference is the function ends with **`5d cb`**
  (pop bp + **retf**) instead of small's `5d c3`
  (pop bp + ret near). Medium has far code, so all
  function returns are far.
- `1870` (**large array via ptr**): pointer ABI
  changes significantly. `int *` is now 4 bytes
  (segment + offset):
  - Callee loads ptr with **`les bx, [bp+6]`** (5
    bytes: `c4 5e 06`) for ES:BX
  - Stores use ES override prefix: **`26 c7 07
    imm16`** (`es: mov word [bx], imm16`, 5 bytes)
  - Each access **reloads** ES:BX (no CSE)
  
  At the call site, BCC uses a **synthetic far
  call**:
  ```
  push ss            ; 16    — push data segment
  lea ax, [x]        ; 8d 46 fc
  push ax            ; 50    — push offset
  push cs            ; 0e    — push CS for retf
  call near _fill    ; e8 disp16 — near call (4B)
  ```
  Total `push cs / call near` = 4 bytes, SHORTER
  than `callf seg:offset` (5 bytes via `9a`). The
  callee's `retf` correctly pops both CS and IP.
  
  This is a Borland-specific optimization: within
  the same segment, fake the far-call protocol
  using cheaper near-call mechanics.

**Cross-model summary**:
| Aspect | Changes per model? |
|--------|---------------------|
| Arithmetic/logic codegen | NO — uniform |
| Register allocation | NO — uniform |
| Boolean/comparison patterns | NO — uniform |
| Encoding policies (imm8-sext, AX-form) | NO — uniform |
| Function `ret` vs `retf` | YES — code-segment-far means retf |
| Pointer width (2 vs 4 bytes) | YES — data-segment-far means 4B |
| Pointer-deref instructions | YES — `mov bx,m / mov [bx]` vs `les bx,m / es: mov [bx]` |
| Synthetic far-call | YES — large/medium use `push cs / call near` |

So **the bulk of the IR-level findings port unchanged**; only the **ABI/pointer boundary** changes per model. This matches the earlier observation from the multi-model fixtures.

For the Rust reimplementation:
- Codegen for non-pointer ops should be model-
  agnostic (shared code path).
- ABI layer: per-model functions for `ret/retf`
  selection, pointer width, push-CS-call-near vs
  callf.

## Large model: `int *` is far automatically; stack arr & enregistration unchanged

Fixtures `1667` (`int *p = &g; *p = 99;`), `1668`
(stack int array), and `1669` (multi-use locals
with longhand assign) extend the large-model
exploration.

- `1667` (**`int *` is auto-far**): in large model,
  `int *p` is automatically a **4-byte far
  pointer**, even without an explicit `far`
  qualifier. `&g` produces a `ds:offset` far
  address — the code emits `mov [bp-2], ds`
  (segment capture via `0x8c /3` mod=11 = mov r/m,
  DS) and `mov [bp-4], 0` (offset, FIXUPP'd). The
  deref-write `*p = 99` uses the standard `les bx,
  [p] / mov es:[bx], 99` path with the `0x26` ES
  prefix. So the small-model `int *p` shape (2-byte
  near ptr, direct `[si]` deref) is replaced with
  the far-ptr shape that was previously seen only
  under the explicit `far` qualifier.
- `1668` (**stack arrays unchanged**): a `int a[3]`
  on the stack with constant indices generates
  **identical code** to small model — `mov [bp+disp],
  imm` for each store, `mov ax, [bp+disp]` for the
  return. Stack-resident data implicitly uses SS, so
  no far-pointer machinery is needed. Only the
  epilogue byte differs (`5d cb` vs `5d c3`).
- `1669` (**register allocation unchanged**): multi-
  use ints still enregister into SI/DI via the same
  use-count heuristic. The `a = a + 1` longhand
  still uses the **AX round-trip** (`mov ax,si /
  inc ax / mov si,ax`) — same as small model's
  fixture `1568`. So **IR-level rules port
  identically** across models. Only the epilogue
  byte (`cb` vs `c3`) reflects the model.

Cross-model rule summary for the code-generation
encoder:
- **IR-level**: register alloc, encoding policies,
  inc/dec optimization, narrow-cast propagation,
  loop normalisation, switch dispatch — **all port
  identically**.
- **ABI-level**: only the call ABI (push cs +
  near vs near alone) and epilogue (`retf` vs
  `ret`) change.
- **Type-level**: `int *` width and codegen path
  change based on data model:
  - Near data: 2-byte ptr, `[si]` deref
  - Far data: 4-byte ptr, `les + 26` deref
- **OBJ structure**: segment naming
  (`_TEXT` → `<MODULE>_TEXT`).

So the multi-model story is **largely orthogonal**
to the deep encoding findings — the encoder needs a
small number of model-conditional emissions.

## Large model (-ml) initial probe: HELLO_TEXT, retf everywhere, push cs + call near

Fixtures `1664` (trivial return-zero), `1665` (call
inc(5)), and `1666` (global int access) are the
**first batch captured under `-ml` (large model)**.
All pass on the first capture. Cross-model
differences vs small (-ms):

- **Code segment name**: `_TEXT` → `HELLO_TEXT`. The
  large model gives each translation unit its own
  uniquely-named code segment, prefixed with the
  module name (uppercased). The SEGDEF and LNAMES
  records reflect this — `_TEXT` is replaced
  throughout. This means we'll need different
  string handling for the segment-name fields per
  model.
- **Function return**: every function uses **`retf`
  (`0xcb`)** instead of near `ret` (`0xc3`). In
  large model, *all* functions are far by default,
  matching the explicit `far` qualifier in small
  model ([[batch-445-pascal-far-fn]]).
- **Function call sites**: intra-module calls use
  **`push cs / call near (e8)`** (4 bytes) instead
  of `call near` alone (3 bytes). The `push cs`
  (`0x0E`) is the standard 1-byte trick that lets
  the callee's `retf` pop both seg+off correctly.
- **Globals**: still accessed via DS-relative
  `a1 disp16` / `a3 disp16` — same as small model.
  Borland's runtime startup sets DS=DGROUP, and as
  long as the program doesn't change DS, globals
  work the same way. The far-data-ness of large
  model shows up only when **`&global`** is taken
  (producing a 4-byte far pointer; not probed yet).
- **Module-flag byte**: at OBJ offset ~0x4d, small
  uses `7f`, large uses `7c`. Different bits in the
  COMENT record's class byte indicating model.

So the multi-model story:
- The IR-level findings (register allocation,
  inc/dec, encoding policies, loop normalisation,
  switch dispatch, etc.) remain unchanged.
- What differs is **fixed prefixes per call**
  (`push cs`), **epilogue bytes** (`retf` not
  `ret`), **segment-name strings**, and
  **module-flag bytes**.

For the Rust reimplementation:
- Plumb a `memory_model` parameter from
  `invocation.toml` (or a new `model` field) down
  to the OBJ emitter.
- Conditionally inject `push cs` before each
  intra-module call when code is far.
- Emit `retf` (`0xcb`) instead of `ret` (`0xc3`)
  for all function epilogues when code is far.
- Use the module-prefixed segment name in SEGDEF /
  LNAMES.

