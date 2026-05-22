# Types and qualifiers

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## `signed` keyword

`signed`, `signed char`, `signed int`, `signed long [int]`,
`long signed [int]` are all accepted by `parse_type` and lower to
the corresponding signed types (`Type::Int`, `Type::Char`,
`Type::Long`). Codegen is identical to the unprefixed forms
since BCC's plain `char`/`int`/`long` are already signed. Fixtures
`467` (`signed char`) and `468` (`signed int`) round-trip to the
same bytes as the unprefixed equivalents — the keyword is purely
front-end.

## `enum <tag>` as a type

In addition to anonymous `enum { A, B, C };` (which only registers
the member constants), `enum <tag>` can now also be used as a type
name in declarations. Fixtures `470`–`472` exercise this as a
global type, local type, and function-parameter type respectively.

Codegen: `enum <tag>` lowers to `Type::Int` (BCC sizes enums as
int). No special storage, comparison, or widening — purely a
front-end alias.

Parser:
- The standalone `enum [<tag>] { … };` dispatcher in `parse_unit`
  now only fires when an opening `{` follows the (optional) tag.
  When the form is `enum <tag> <decl>` the dispatcher skips and the
  type-prefix path handles it.
- `parse_type` learned `enum [<tag>]` → `Type::Int` (the tag is
  consumed if present but we don't require it to be in any tag
  table — the enum members were registered at the definition site).
- The top-level type-probe gained an `enum [<tag>]` arm.
- `parse_stmt`'s declaration dispatch now accepts `KwEnum` (and
  `KwSigned`, completing the set started in batch 50) as a type
  start, so `enum color c;` works inside function bodies.

Explicit member values (`enum flag { OFF = 0, ON = 1, AUTO = 7 }`)
also flow through the same path — the body parser already accepted
`= <int-lit>` per-member, and the values fold into `IntLit` at use
sites (fixture `473`).

The body form `enum [<tag>] { … } <decl>` (combined definition +
declaration) works too. Fixture `474`'s
`enum { A = 1, B = 2, C = 3 } x;` declares both the constants and
a local `x`. Implementation factored `parse_enum_body` out of
`parse_enum_decl` so `parse_type`'s enum branch can reuse it; the
caller consumes through `}`, then the surrounding declare path
sees the declarator.

## `const` / `volatile` / `register` qualifiers

`const`, `volatile`, and `register` are accepted as discardable
qualifiers — BCC keeps the storage layout identical to the
unqualified form. Fixtures `475` (const global), `476` (volatile
global), and `477` (register local) all round-trip to bytes that
match the equivalent unqualified declaration.

Implementation: a single `while` loop at the top of `parse_type`
consumes any combination of these three keywords, and a parallel
consumer runs at the start of the top-level type-probe. All three
are also accepted as type-starts in `parse_stmt`'s declaration
dispatch. No AST node — the qualifiers are just dropped.

Note: BCC's actual `register` keyword is a *hint* that forces
enregistration even below the natural use-count threshold (the
oracle for `register int x; x = 5; g = x;` enregisters `x` into
SI even though `x` only has 2 uses, below the int-enregistration
threshold of 3). We don't yet honor that hint — fixture `477` uses
`x` three times so it enregisters naturally; if a real fixture
requires register-hint enregistration the allocator will need a
new bias channel.

### Register-resident int → global store

While unblocking fixture `477` a separate gap turned up: `g = x`
where `x` is a register-resident int local was emitted as
`mov ax, si / mov word ptr [_g], ax` (5 bytes, AX round-trip).
BCC emits the direct `mov word ptr [_g], si` (`89 36 disp16`, 4
bytes). `emit_assign_global` now special-cases register-resident
int RHS to use the register-source-to-global form via the existing
`MovGroupSymReg16` instruction.

## `volatile`/`const` accepted but no-ops at codegen; `register` is effective (enregisters)

Fixtures `2243` (volatile), `2244` (const), `2245`
(register) test C qualifier handling.

- `2243` (**volatile**): emits **two separate
  loads** of x for `a = x; b = x;`:
  ```
  mov ax, [x] / mov [a], ax
  mov ax, [x] / mov [b], ax    ; reloaded (not cached)
  ```
  But BCC doesn't do CSE/load-caching anyway, so
  this is what would happen WITHOUT volatile too.
  Effectively a no-op at codegen.
- `2244` (**const local int**): N is given a
  normal stack slot, loaded from memory for
  comparison:
  ```
  mov word [N], 10
  ; ... later:
  cmp si, [N]                  ; not "cmp si, 10"
  ```
  BCC does NOT fold const-qualified variables to
  compile-time literals. Const is purely a type-
  system marker (for diagnostic warnings).
- `2245` (**register int**): variable goes into
  SI (or DI) — effective:
  ```
  mov si, 1                    ; i = 1 (in SI register)
  ; loop body uses SI directly
  ```
  Confirms that `register` is the ONE qualifier
  that actively affects codegen.

**C qualifier handling summary**:
| Qualifier | Codegen impact |
|-----------|----------------|
| `volatile` | None (BCC doesn't cache anyway) |
| `const` | None (BCC doesn't fold) |
| `register` | Hints enregistration (SI/DI/CX/...) |
| `static` | Changes symbol export (no PUBDEF) |
| `extern` | Changes symbol export (EXTDEF, no slot) |
| `auto` | Default for locals (no effect) |
| `near` | Forces near ptr (2B) in non-small models |
| `far` | Forces far ptr (4B) in non-huge models |
| `cdecl` | Default calling convention |
| `pascal` | Reverses arg order; callee cleanup |
| `interrupt` | Saves/restores all regs; iret |

So most modifiers don't change codegen at all —
they affect typechecking or symbol-table state.
Only `register`, `near`/`far`, and the calling
conventions actually shape code emission.

**Why volatile is a no-op in BCC**:
BCC is a simple compiler that performs:
- Parse-time constant folding
- Parse-time identity folding (x+0, x*1)
- Parse-time pow-2 strength reduction
- Per-statement register allocation

It does NOT perform CSE, DCE, loop hoisting, or
load forwarding. So `volatile` has nothing to
suppress.

For the Rust reimplementation:
- Track qualifiers in the type system.
- Honor `register` for enregistration hint.
- `volatile`/`const` codegen = same as without.

## `asm { ... }` block syntax; `#pragma warn` PP-only; pseudo-registers `_AX`/etc. in C exprs

Fixtures `2120` (asm block), `2121` (pragma warn),
`2122` (pseudo-reg + asm) cover BCC extensions.

- `2120` (**`asm { ... }` block**): multi-line
  inline assembly in braces. Each line emits one
  instruction. Equivalent to multiple `asm
  <instr>;` statements:
  ```c
  asm {
    mov ax, x
    add ax, 5
    mov x, ax
  }
  ```
  Output: `8b 46 fe / 05 05 00 / 89 46 fe` (9
  bytes). Same as the separate-line form.
- `2121` (**`#pragma warn -ccc`**): disables a
  specific warning class at PP level. **No OBJ
  effect** — the directive only influences the
  compiler's warning emission, not the code.
- `2122` (**pseudo-registers `_AX`, `_BX`, ...**):
  Borland-specific: `_AX` in a C expression reads
  the current AX register value. Combined with
  inline asm to do byte-swap:
  ```c
  asm mov ax, x          // load x into AX
  asm xchg ah, al        // swap bytes (86 c4)
  return _AX;            // return current AX
  ```
  No `mov ax, ax` shuffling — `_AX` directly
  exposes the register. Result: `0x1234` → `0x3412`
  (byte-swapped).

**Pseudo-register summary** (BCC extension):
| Pseudo | Register |
|--------|----------|
| `_AX`, `_BX`, `_CX`, `_DX` | 16-bit GP |
| `_AL`, `_BL`, `_CL`, `_DL` | low 8-bit halves |
| `_AH`, `_BH`, `_CH`, `_DH` | high 8-bit halves |
| `_SI`, `_DI`, `_BP`, `_SP` | index/stack |
| `_CS`, `_DS`, `_SS`, `_ES` | segment |
| `_FLAGS` | flags word (some variants) |

These are useful when interfacing inline asm with
C code — the C statement after the asm can pick
up the asm's result without an explicit `mov` to
a C variable.

For the Rust reimplementation:
- Parse `asm { ... }` block syntax.
- Pragmas: emit as no-op directives, with side
  effects on warning state.
- Pseudo-registers: parse as primary expressions
  that map directly to register operands.

## 7 locals: only 4 enregister (DX reserved); nested calls use arg-stack; mixed cmp via cast

Fixtures `1973` (7 multi-use locals), `1974`
(`f(g(...), h(...))` nested), `1975` (mixed
signed/unsigned cmp) cover more register-
allocator behavior.

- `1973` (**7 locals → only 4 enregister**):
  with 8 multi-read candidates (7 locals + 1
  derived `r`), BCC enregisters **only 4**:
  - `a` → DI
  - `b` → BX
  - `c` → CX
  - `r` → SI (the accumulator)
  - `d, e, f, g` → stack
  
  So **DX is NOT used** for a local. Likely
  reserved as scratch (especially because the
  function uses `imul`, which produces the high
  half in DX). With imul present, the pool
  effectively becomes 4: {SI, DI, BX, CX}.
  
  Refined rule:
  - Without imul/idiv: pool = {SI, DI, BX, CX, DX}
    (5 slots, see [[batch-511-five-locals]])
  - With imul/idiv: DX reserved, pool = {SI, DI,
    BX, CX} (4 slots)
- `1974` (**nested calls use arg-stack as scratch**):
  `f(g(...), h(...))` evaluates right-to-left:
  ```
  ; compute h(3, 4):
  push 4 / push 3 / call h / pop / pop          ; ax = 7
  push ax                                        ; save as outer's 2nd arg
  ; compute g(1, 2):
  push 2 / push 1 / call g / pop / pop          ; ax = 3
  push ax                                        ; save as outer's 1st arg
  ; call f:
  call f / pop / pop                             ; ax = result
  ```
  Each inner call's result is **pushed directly
  as the corresponding arg of the outer call**.
  No temporary stack variables; the args stack
  doubles as scratch.
- `1975` (**mixed signed/unsigned cmp via cast**):
  `(int)u` makes the comparison **signed**:
  ```
  mov ax, [s]                ; -1 (= 0xffff)
  cmp ax, [u]
  jge L_false                ; signed jge for inverse of <
  ```
  For s = -1, u = 1: signed `-1 < 1` is TRUE
  (return 1). Unsigned `0xffff < 1` would be
  FALSE. The cast forces signed-cmp semantics.
  
  Type-driven jcc choice: BCC tracks the type of
  each operand and chooses the appropriate jcc.

**Refined register-allocation pool**:
- No mul/div: {SI, DI, BX, CX, DX} = 5 slots
- With imul/idiv: {SI, DI, BX, CX} = 4 slots
  (DX reserved as imul's high-half target)

For the Rust reimplementation:
- Track whether the function uses mul/div ops;
  reserve DX accordingly.
- Nested calls: use args-stack as scratch for
  inner results.
- Cast-driven cmp signedness: track operand
  types through casts.

## K&R `()` = `(void)`; `extern int` in EXTDEF + FIXUPP; expr-stmt computes but discards

Fixtures `1958` (K&R `()` fn decl), `1959`
(`extern int`), `1960` (discarded expr-stmt)
cover three smaller language features.

- `1958` (**K&R `()` = `(void)`**): `int get42()
  { return 42; }` with no `void` keyword
  compiles identically to `int get42(void)`.
  No proto means no arg-type checking — but
  for zero-arg fn definitions the codegen is
  the same. Call site emits no pushes, no
  cleanup.
- `1959` (**`extern int outer`**): the extern
  decl adds **`_outer` to EXTDEF** (external
  symbol table). Access uses **`a1 0000`** (mov
  ax direct address, with FIXUPP) — link-time
  resolves the imm16 to outer's actual offset.
  
  No storage allocated in this TU; another TU
  must define `outer`. The OBJ requires linking
  with a definition.
- `1960` (**discarded expr-stmt**): `x + 1;`
  (statement with unused result) **emits the
  computation**:
  ```
  mov ax, si       ; load x
  inc ax           ; compute x + 1
  ; (result in AX, discarded — no store)
  ```
  Wasteful but consistent with BCC's "compile
  each statement" rule. The increment is
  computed but not stored.
  
  Subsequent `x++` properly increments via `inc
  si` (the variable's register slot).

So **no dead-code elimination** even for
side-effect-free expressions. Every C source
expression generates instructions, regardless
of usefulness.

For the Rust reimplementation:
- K&R `()` fn defn: treat as `(void)` (no args)
  for codegen purposes.
- `extern` decls: add to EXTDEF, generate
  FIXUPPs at use sites.
- Expression statements: emit the computation
  even if result is discarded. No DCE.

## 5-locals fills full pool; 1-read locals stack; params enregister too

Fixtures `1850` (5 locals), `1851` (3 locals
across call, all 1-read), and `1852` (function
params enregistering) refine the register-
allocation rule.

- `1850` (**5 multi-use locals fill full pool**):
  with 5 locals all needing slots, BCC uses **all
  5 registers**: SI, BX, DI, CX, DX (in
  declaration order, with the pool {SI, DI, DX, BX,
  CX}). No scratch reserved when register pressure
  is high. Confirms the pool's full extent.
  
  Notable: declaration order maps to register
  selection: a→SI, b→BX, c→DI, d→CX, e→DX.
  This may suggest an internal pool order:
  ```
  {SI, BX, DI, CX, DX}
  ```
  (or perhaps {SI, BX, DI, CX, DX} as the BCC
  fill-order — need more probes to confirm).
- `1851` (**1-read locals stay on stack**): 3
  locals each used only once (init + return) all
  stay on stack — even though the function makes a
  call. The threshold for enregistration is firmly
  **≥ 2 reads** in expressions, regardless of
  surrounding context.
- `1852` (**params enregister too**): function
  parameters with ≥ 2 reads also enregister.
  In `do_calc(a, b, c, d)` where a is used twice
  (in `x = a+b` and in `return ...+a`) and d is
  used twice, BCC loads a→SI and d→DI in the
  prologue:
  ```
  mov si, [bp+4]    ; a → SI
  mov di, [bp+10]   ; d → DI
  ```
  Then uses SI/DI throughout the body. Params and
  locals follow the same enregistration rules.

For the Rust reimplementation:
- Count reads (uses in expression contexts) for
  every local AND parameter.
- Variables with ≥ 2 reads enregister using the
  full pool {SI, BX, DI, CX, DX}.
- Prologue includes loads for enregistered params
  (`mov reg, [bp+disp]`).

## Medium = far-code, near-data; Compact = near-code, far-data; segment capture varies

Fixtures `1766` (medium `-mm` fn call), `1767`
(compact `-mc` fn call), and `1768` (compact &global)
extend cross-model coverage to all 4 standard
memory models.

- `1766` (**`-mm` medium**): far code, near data.
  - Segment name: **`HELLO_TEXT`** (like -ml).
  - Function ABI: `retf`, args at `[bp+6]`, `push
    cs ; call near` at sites. **Identical to -ml
    for code-only ops**.
  - Data ABI: would be near (DS-relative) since
    "near data" is the model — but this fixture
    doesn't touch globals.
- `1767` (**`-mc` compact**): near code, far data.
  - Segment name: **`_TEXT`** (like -ms).
  - Function ABI: `ret` (near), args at `[bp+4]`,
    `call near` at sites. **Identical to -ms for
    code-only ops**.
  - Data ABI: would be far when address-taken.
- `1768` (**`-mc` `&global`**): confirms compact's
  far-data nature. `int *p = &g` produces a **4-
  byte far pointer**:
  ```
  mov [bp-2], ds        ; 8c 5e fe — capture DS (not SS!)
  mov [bp-4], &g        ; FIXUPP'd offset
  les bx, [p]
  mov es:[bx], 42        ; 26 prefix
  ```
  Notable: the segment capture is **`mov [m], ds`**
  (`8c /3`) since g lives in DS, not `mov [m], ss`
  (`8c /2`) like for stack-local addresses. Function
  itself uses near `5d c3` ret.

Final memory-model matrix:
| Model | Code | Data | Seg name | retX | call site |
|-------|------|------|----------|------|-----------|
| `-ms` small | near | near | `_TEXT` | ret | call near |
| `-mc` compact | near | far  | `_TEXT` | ret | call near |
| `-mm` medium | far  | near | `HELLO_TEXT` | retf | push cs / call near |
| `-ml` large | far  | far  | `HELLO_TEXT` | retf | push cs / call near |

And **segment-register capture by storage class**:
| Storage | Segment | Capture opcode |
|---------|---------|----------------|
| stack (local) | SS | `8c /2` |
| global / static (DS) | DS | `8c /3` |
| code (CS) | CS | `8c /1` (rare) |
| ES, GS, FS | — | `8c /0`, etc. |

For the Rust reimplementation:
- `code_model: Near|Far` → controls call/ret + push cs + seg name + arg offset base.
- `data_model: Near|Far` → controls pointer width and seg capture for & operator.
- These are **independent** parameters — small=NN, compact=NF, medium=FN, large=FF.

## `register` keyword forces enreg; use-count breaks ties; `&x` forces stack

Fixtures `1763` (register keyword), `1764` (varying
use counts), and `1765` (address-taken var)
clarify the register allocation policy further.

- `1763` (**`register` keyword**): `register int
  n;` explicitly enregisters n into SI even though
  there are 5 locals total. The `register` hint
  takes priority over other selection criteria,
  guaranteeing the variable gets a register slot.
- `1764` (**3 locals, all enregister**): with
  exactly 3 multi-use locals (no spills), each
  gets its own register: rare→DI, often→SI,
  seldom→DX. So the register pool ordering for
  3 simultaneous locals is **{DI, SI, DX}** when
  no register-keyword hints are present.

  Combined with `1760`'s observation (`a→SI,
  c→DI, e→DX` when a, c, e win the 3-slot lottery
  against b, d, f):
  - When all qualifying locals fit, the
    declaration-order maps to a specific register
    sequence.
  - When more qualify than fit, **use-count breaks
    the tie**: locals with higher read-count win
    register slots over locals with lower counts.
  - In 1760, a/c/e have 3 uses each, b/d/f have 2
    uses each — so the 3-use group wins all 3
    slots regardless of declaration order parity.
  - In 1764, all 3 qualify; assignment is DI/SI/DX
    by declaration order (though precise mapping
    may depend on register pressure analysis).

- `1765` (**`&x` forces stack**): when `&x` is
  taken, x **stays on the stack** regardless of
  use count — needed so the address is addressable.
  `*p = *p + 3` becomes:
  ```
  mov ax, [si]       ; load *p
  add ax, 3
  mov [si], ax       ; store *p
  ```
  And `x = x + 1` becomes a memory-RMW:
  ```
  mov ax, [bp-2]
  inc ax
  mov [bp-2], ax
  ```
  The pointer p, however, enregisters into SI
  (it's an automatic local without taken address).

Updated register-allocation rule (final):
1. **Mandatory enregistration** (override pool
   limits): `register` keyword.
2. **Mandatory stack**: `&var` taken, `volatile`
   qualifier.
3. **Candidates**: remaining locals with read-count
   ≥ 2 in expressions (init/single-write doesn't
   count).
4. **Selection**: up to 3 candidates win register
   slots. When more qualify than slots:
   - Higher use-count breaks ties first.
   - Among equal use counts, declaration order
     (earliest wins).
5. **Pool**: {SI, DI, DX}. BX/CX reserved for
   scratch.

## `far` pointers: 32-bit seg:off, `les` + `26` ES override

Fixture `1649` (`int far *p = (int far *)&x; return
*p;`) compiles cleanly in small model and reveals
the **far-pointer codegen** model:

- A `far` pointer is **32 bits** (2 words on stack):
  - Lower word: offset
  - Higher word: segment
- Constructing a far pointer from a near address
  uses **`mov [seg_slot], ss`** (opcode `8c /2`,
  `mov r/m16, SS`) to capture the local's segment
  (which is `SS` for stack-allocated `x`), then a
  `mov [off_slot], ax` for the lea'd offset.
- Loading the far pointer for deref uses **`les bx,
  [bp+disp]`** (opcode `c4 /r`, "Load far pointer
  into ES:reg") — single instruction loads both
  offset into BX and segment into ES from the 4-byte
  source.
- The actual memory access through the far pointer
  uses an **`ES:` segment-override prefix** (byte
  `0x26`): `26 8b 07` = `mov ax, es:[bx]`.

So the lowering pattern for `int far *p; *p = ...`:
```
8c 56 disp   ; mov [p_seg], ss          (or other seg)
89 46 disp   ; mov [p_off], ax
c4 5e disp   ; les bx, [p]
26 8b 07     ; mov ax, es:[bx]          (or write equivalent)
```

For the Rust reimplementation: the far-pointer
support is a known Borland extension — needs:
- Recognising `far`/`near`/`huge` type qualifiers in
  the parser.
- Treating `int far *` as a 32-bit type (4 bytes,
  word-aligned with high half = segment).
- Emitting `8c /2` for segment captures, `c4 /r`
  for far-pointer loads, and `26` prefixes for ES-
  based memory access.
- Stack locals' addresses naturally have `SS` as
  segment; global/static would use `DS` (i.e. `8c
  56 disp` for stack and `8c 5e disp` for DS-based
  globals).

## Function params enregister like locals (use-count ≥ 2 → SI/DI/...)

Fixtures `1526` (param `x` used 3x: `x*x + x`),
`1527` (param `x` used 2x: `x+x`), and `1528` (two
params `a` and `b` each used 2x in `(a-b)*(a+b)`)
all pass on the first capture and extend the
enregistration model to function parameters.

- `1526`: `_f(int x)` reads from `[bp+4]` (the cdecl
  first-arg slot) **once** into SI on entry — `mov
  si, [bp+4]`. All three uses of `x` (`x*x` first
  factor, `x*x` second factor, the trailing `+x`)
  then operate on SI. So a multi-use param is
  promoted into a register, the same as a multi-use
  local. The arg slot at `[bp+4]` is never reloaded.
- `1527`: `_f(int x)` with `x+x` (2 uses) similarly
  enregisters `x` → SI via `mov si, [bp+4] / mov
  ax,si / add ax,si`. Confirms the threshold is the
  same ≥ 2 syntactic uses, including for parameters.
- `1528`: two parameters, each used twice. **`a` →
  SI** (`mov si, [bp+4]`), **`b` → DI** (`mov di,
  [bp+6]`). Declaration order matches the
  register-allocation order. The intermediate `(a+b)`
  is computed into DX (a scratch register) before the
  `imul`. Confirms params occupy `[bp+4]`, `[bp+6]`,
  ... in cdecl, and that BCC's allocator treats them
  uniformly with locals — the use-count heuristic
  doesn't distinguish param-from-local.

Implication for the encoder: when a function body
has multi-use parameters, BCC always emits the
`mov REG, [bp+N]` copy on entry (after the prologue
push of REG), and then never touches the stack slot
again. The Rust reimplementation needs to walk the
function body to classify each parameter's syntactic
use count *before* emitting the prologue.

## `int b = a++`, `int b = --a`, void setter via global

Fixtures `1319` (`int a=5; int b = a++; return b;` —
int post-increment as the RHS of an initializer),
`1320` (`int a=5; int b = --a; return b;` — int prefix
decrement as the RHS of an initializer), and `1321`
(`int g; void set(int v) { g = v; } set(42); return
g;` — void setter that writes a global from its arg)
all pass on the first capture. `1319` confirms the
postfix-`++` in init expression position works
identically to the regular RHS shape (`1265`): load
pre-value into AX, store into `b`, then `inc` the
source slot. `b=5, a=6`. `1320` confirms the prefix-
`--` in init: `dec word ptr [bp-a]`, then load the
*post*-decrement value into AX and store. `b=4, a=4`.
`1321` confirms void-returning setter: the callee
doesn't load AX before its `pop bp / ret`, so the
caller sees whatever AX held at the call site (here
discarded since the call is statement-position). The
global `g` is updated, then `main` returns its value.

## `f(a++)`, int cmp hex const, `strlen` as fn

Fixtures `1265` (`int a=5; return f(a++);` — int
post-increment used as a call argument), `1266` (`int
a=0xff; if (a > 0x80) return 1;` — int compared to a
hexadecimal constant), and `1267` (`int len(char *s) {
int n=0; while (*s) { n++; s++; } return n; }
return len("abc");` — strlen-style function whose body
traverses a `char *` until it sees null) all pass on
the first capture. `1265` confirms the postinc-as-arg
shape: load `a` into AX, push, then `inc word ptr
[a]` afterward — the pushed value is the pre-increment
value, matching the postfix semantic. `1266` confirms
hex constants fold to identical bytes as decimal:
`0x80` becomes `128`, and the compare emits `cmp ax,
128` -- the parser normalizes hex literals before
codegen sees them. `1267` confirms the strlen idiom:
the while body is a `cmp byte ptr [bx],0 / je END`
exit test (using `bx` for the pointer), with `inc bx`
as the step. The call site passes the literal "abc"
pointer through the standard cdecl push, then reads
length from AX.


## `int` ↔ `unsigned int` cast — type-only, no codegen (fixture `2387`)

`unsigned int u = (unsigned int)x;` for an `int x` emits **no
conversion code** — the cast is byte-identical to a plain assign:

```c
int x = -5;
unsigned int u = (unsigned int)x;
return (int)(u / 2);
```

```
c7 46 fe fb ff          ; x = -5 (0xFFFB)
8b 46 fe                ; mov ax, x
89 46 fc                ; u = ax     ← cast bytes = plain assign
8b 46 fc                ; mov ax, u
d1 e8                   ; shr ax, 1  ← unsigned div by 2 = shr (NOT sar)
```

What matters is that the cast **propagates the type** for later
operations. The subsequent `u / 2` lowers to `shr ax, 1` (the
unsigned shift, opcode `/5` = `d1 e8`) rather than `sar ax, 1`
(`/7` = `d1 f8`) because `u`'s declared type is `unsigned int` at
that point.

Without the cast, treating x = -5 as signed and dividing by 2 would
give `sar` semantics (= -3). With the cast, it's unsigned division
(0xFFFB / 2 = 0x7FFD = 32765). So the cast carries type information
through the assignment, even though no bytes change.

Confirms: signed↔unsigned casts on same-width integers are
**type-system-only** in BCC. They steer opcode selection (`shr` vs
`sar`, `div` vs `idiv`, `jbe` vs `jle`) for downstream operations
but emit nothing at the cast site.

## `static const int K = 100;` — NOT constant-propagated (fixture `2492`)

`const`-qualified variables in BCC are **stored in memory** and
**loaded on each use**, like any non-const variable. BCC does NOT
fold uses of `K` into a literal at the call sites.

```c
static const int K = 100;
return K + 5;
```

```
a1 00 00                ; mov ax, [_K]   ← FIXUPP'd memory read
05 05 00                ; add ax, 5
```

Compare with a hypothetical constant-propagation pass that would
emit:

```
b8 69 00                ; mov ax, 105     ← K + 5 folded at compile time
```

C90 strictly: `const int K` is NOT an integer constant expression
(per §6.6 — only enums and macros are). So a conforming compiler
isn't required to fold. BCC complies but goes no further: even
when the const initializer is a literal known at parse time, the
fold doesn't happen.

So `const` in BCC is purely a **type-system flag** that prevents
writes to the variable. Codegen-wise:
- `int x = 100;` → store to `_DATA`, loads via FIXUPP
- `const int x = 100;` → **byte-identical** OBJ (load via FIXUPP)
- `static const int K = 100;` → same as `static int K = 100;` plus
  PUBDEF suppression (static effect)
- `#define K 100` → fully inlined; uses become `mov ax, 100`-style
  literals

For genuinely-constant integer use cases, `#define` or `enum {K =
100};` give better codegen than `const int`. BCC's behavior matches
the C90 standard letter, not the spirit of "const = compile-time-
known".
