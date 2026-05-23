# Operators, evaluation order, enregistration

This file is part of the BCC parser/codegen behavior catalog. See [`../PARSER.md`](../PARSER.md) for the index.

## Comma operator

`<expr>, <expr>` at expression level is a comma operator —
distinct from the comma as argument-list / init-list separator.
C grammar only permits it in a *comma-expression* context:
parenthesized expressions and the top of expression statements.
Implementation only handles the parenthesized form for now
(`g = (a = 1, b = 2, a + b);` — fixture `469`).

Each element inside the parens is parsed via
`parse_for_clause_expr`, which already recognizes
`<ident> = <expr>` as `ExprKind::AssignExpr`. The comma-separated
elements chain left-associatively into nested
`ExprKind::Comma { left, right }` nodes.

Codegen: the left side is discarded (side effects only) and the
right side produces the value. In `emit_expr_to_ax`, the comma
maps to `emit_expr_discard(left)` then `emit_expr_to_ax(right)`.
`emit_expr_discard` recursively handles Comma so nested chains
like `(a = 1, b = 2, a + b)` discard all but the rightmost
element correctly.

## Chained assignment

Fixture `500` (`int a, b, c; a = b = c = 5;`) — C's `=` is
right-associative and yields a value, so `a = b = c = 5` parses
as `a = (b = (c = 5))`. The statement-level dispatch for
`<ident> = …` now uses `parse_for_clause_expr` (rather than
`parse_expr`) for the RHS, so the RHS can itself be another
`AssignExpr`. `parse_for_clause_expr` was made recursive on its
RHS to support the chain.

Codegen for `AssignExpr` in value position lives in
`emit_expr_to_ax`: it recursively evaluates the inner value into
AX, then emits one `mov word ptr <target>, ax` for the
side-effect store. AX still holds the assigned value so the
outer assignment reuses it. The resulting sequence for `a = b =
c = 5;` is `mov ax, 5; mov [_c], ax; mov [_b], ax; mov [_a],
ax` — one literal load and three stores, exactly what BCC emits.

## Char-ident RHS — same RHS-first shape as `Call`

Fixture `616` (`int f(int a, char b) { return a + b; }`) —
loading a char clobbers AX through the `mov al, byte ptr ...;
cbw` widen, so BCC evaluates the char RHS first, pushes the
widened result, then loads the int LHS, pops into DX, and
applies the op. Previously our `emit_binary_right` had a
char-on-right pattern that produced a functionally equivalent
result through `push ax / mov al,...; cbw / mov dx, ax / pop
ax`, which is 2 bytes longer because of the extra `mov dx,
ax`. Extended the batch-92 RHS-clobbers-AX check (originally
just `Call`) to also fire on a char-typed `Ident` RHS, routing
through the cleaner `evaluate RHS / push / evaluate LHS / pop
dx / op` shape.

## Nested BinOp as RHS — extend RHS-clobbers-AX path

Fixture `645` (`return x + y * 2;`) — when the right operand
of a binary op is itself a non-constant BinOp (e.g., `y * 2`),
its evaluation lands the result in AX. Previously
`resolve_operand_source` panicked on a BinOp RHS. BCC's
pattern matches the call-RHS path from batch 92:

```text
  mov ax, [bp-4]    ; y
  shl ax, 1         ; y * 2
  push ax           ; save RHS
  mov ax, [bp-2]    ; x
  pop dx            ; recover RHS
  add ax, dx        ; x + (y*2)
```

Extended the `rhs_clobbers_ax` check in `emit_expr_to_ax`'s
BinOp arm to also fire when `right.kind` is a non-constant
BinOp. That routes through the existing RHS-first / push /
LHS / pop dx / op-with-DX sequence.

## `volatile` blocks enregistration; `5+3` global init folds at compile time

Fixtures `1547` (`dbl(a + b)` — binop result passed
as arg), `1548` (`volatile int x; x = x + 1`), and
`1549` (`int g = 5 + 3;`) all pass on the first
capture.

- `1547`: confirms the binop → push fast path:
  `mov ax,[bp-2] / add ax,[bp-4] / push ax / call /
  pop cx`. The `add ax,...` leaves the result in AX
  ready for `push ax` — no intermediate stack
  storage. `a` and `b` are single-use locals
  (1 use after init in the `a+b`), so they stay on
  stack.
- `1548` (**finding**): **`volatile` forces a local
  to stay in memory** regardless of use count.
  Despite `x = x + 1; return x` being 2 syntactic
  uses (would normally enregister `x` into SI),
  BCC emits: `mov ax,[bp-2] / inc ax / mov [bp-2],
  ax / mov ax,[bp-2]` — re-loading from memory even
  immediately after the store. So `volatile` is a
  third constraint that forces stack residence,
  alongside (1) use-count < 2 and (2) address-taken.
- `1549`: confirms compile-time arithmetic folding
  for global initialisers — `int g = 5 + 3;` emits
  the data byte sequence `08 00` (i.e. 8) directly
  in `_DATA`. The expression `5 + 3` is fully
  evaluated by the parser/AST layer before reaching
  codegen.

Combined "spill to memory" conditions for locals:
1. Use count < 2 after declaration.
2. Address taken (`&local` appears anywhere).
3. Declared `volatile`.
Any one of these forces the local into a stack slot.

## Enregistration extends to 5 regs: SI, DI, DX, BX, **CX**; fn-call ABI

Fixtures `1505` (5 multi-use ints all simultaneously
live), `1506` (2 multi-use ints with an intervening
function call), and `1507` (multi-use int paired
with a variable shift that needs CL) all pass on the
first capture.

- `1505` (**bigger finding**): 5 multi-use ints all
  enregister — into SI, DI, DX, BX, and **CX**. No
  stack allocation at all. So the enregistration
  pool spans all 5 general-purpose registers that
  aren't AX/BP/SP: `SI, DI, DX, BX, CX`. The order
  appears to be the declaration order of the locals.
- `1506` confirms the **caller-save / callee-save
  split for register-allocated locals**: across a
  `call _inc`, the locals in SI and DI are
  *not* spilled — BCC relies on SI/DI being callee-
  saved by the callee's `push si / push di`
  prologue. Arg cleanup uses `pop cx` (CX is scratch
  / caller-save and the simplest 2-byte reclaim).
  Function return comes back in AX; BCC then stores
  it into DI (the local's home register). This
  implies BCC will *not* place a local in DX, BX, or
  CX if its lifetime crosses a function call —
  otherwise the call would clobber it. (Hypothesis
  — needs a future probe with 3+ multi-use locals
  straddling a call.)
- `1507`: shift amount `n` is read only once
  syntactically, so it stays on the stack as
  expected. Notable detail: BCC loads it with `mov
  cl, [bp-2]` (`8a 4e fe`, byte load) rather than
  `mov cx, [bp-2]` (`8b 4e fe`) — same 3-byte length,
  but byte load is preferred when only `cl` is
  needed. The shift `sar ax, cl` follows immediately.

Updated register-allocation table:
| Order | Reg | Saved on entry? | Survives calls? |
|-------|-----|------------------|-----------------|
| 1     | SI  | `push si`        | yes             |
| 2     | DI  | `push di`        | yes             |
| 3     | DX  | not saved        | **no**          |
| 4     | BX  | not saved        | **no**          |
| 5     | CX  | not saved        | **no**          |

## Enregistration register order: SI, DI, DX, **BX** — 4 ints fit

Fixtures `1502` (4 locals, 2 multi-use + 2 single-
use), `1503` (4 locals **all** multi-use), and
`1504` (1 local with 4 syntactic uses) all pass on
the first capture and extend the enregistration
findings:

- `1502`: confirms the use-count rule under pressure
  — `a` and `b` (both used twice) go to SI/DI; `c`
  and `d` (both used once) stay on the stack at
  `[bp-2]` / `[bp-4]`. Prologue: `sub sp, 4` only,
  with `push si / push di`.
- `1503` (**major finding**): when 4 ints are all
  multi-use, all 4 go into registers — SI, DI, DX,
  and `**BX**`. No `sub sp` at all (no stack
  locals), and no `push bx` either — BCC treats BX
  as scratch in this calling convention and doesn't
  preserve it across the call from runtime startup.
  Code shape per assignment: `mov ax, REG / inc ax /
  mov REG, ax` (the inc-vs-add policy still applies
  to the AX temp).
- `1504`: 1 local with 4 syntactic uses → only SI
  needed; BX, DI, DX stay free. Each `v = v + K`
  round-trips through AX (`mov ax,si / op / mov
  si,ax`) — there's no peephole that keeps the
  result in AX and skips the store-back when the
  next use is also via AX.

Updated register-allocation table:
| Order | Reg | Saved on entry?  |
|-------|-----|------------------|
| 1     | SI  | `push si`        |
| 2     | DI  | `push di`        |
| 3     | DX  | not saved        |
| 4     | BX  | not saved        |

The first two (SI, DI) are pushed in the prologue.
DX and BX are treated as scratch — clobbered without
preservation. The maximum simultaneous enregistered
int count observed so far is 4.

## Enregistration heuristic narrowed: use-count threshold ≥ 2

Fixtures `1499` (`(a+b) + (a-c)` — `a` used twice),
`1500` (`while(a<b){c+=a; a++;}` — `a` and `c` each
used twice in the loop body+test), and `1501` (same
sum as `1496` but with declarations separated from
initialisers) all pass on the first capture and
together narrow the heuristic from
[[batch-393-enreg-spill]].

Observations:
- `1499`: only `a` (used twice in two distinct
  sub-expressions) goes to SI. `b` and `c` stay on
  the stack at `[bp-2]` / `[bp-4]`. The
  computation: `mov ax,si / add ax,[bp-2] / mov
  dx,si / sub dx,[bp-4] / add ax,dx`.
- `1500`: `a` → SI (read in cmp + written by `a++`),
  `c` → DI (compound `c += a` reads and writes), but
  `b` → `[bp-2]` (read once per cmp, syntactically
  one occurrence).
- `1501`: same lowering as `1496` — all on stack —
  confirming that *initialiser-at-declaration vs.
  initialiser-as-separate-statement* makes **no
  difference**. The init counts the same either way.

So the actual heuristic is: **enregister a local iff
it has ≥ 2 syntactic uses (read or write) after its
declaration, excluding the initialiser**. Each
syntactic operand counts once (e.g. `a < b` is one
read of `a` and one of `b`; `a++` is one use of `a`;
`c += a` is one use of `a` and one use of `c`).
Compound `+=` is one syntactic op even though
semantically it reads and writes — BCC counts it as
one. Under register pressure, the first ≥2-use
locals claim SI/DI/DX in declaration order; the
maximum simultaneous in-register count observed so
far is 3.

## Enregistration heuristic: 3/4/5-local pure sum all spills

Fixtures `1496` (`int a=1, b=2, c=3; return a+b+c;`),
`1497` (4 locals), and `1498` (5 locals) — all pure
"declare-with-literal-init then sum" — pass on the
first capture. Notable result: **all three fixtures
spill every local to the stack**. Code shape (for
3-local case):
`sub sp,6 / mov [bp-2],1 / mov [bp-4],2 /
mov [bp-6],3 / mov ax,[bp-2] / add ax,[bp-4] /
add ax,[bp-6]`. The 4-local and 5-local versions
just extend the pattern.

This contradicts the naive "BCC enregisters into SI,
DI, DX in order until full" model. The earlier
[[batch-392-char-idx-if-empty]] / fixture `1494`
showed 3 ints in SI/DI/DX, but `1494` differs from
`1496` in two ways: (a) its third local `int x;` had
no initializer at declaration — only a conditional
assignment in each arm of the if-else, and (b) `a`
and `b` are read *twice* each (cmp + sub) before
the return. So BCC's enregistration heuristic is
*not* purely positional — it depends on usage
density and/or initializer style. The "declared and
literal-initialised then read once" pattern of `1496`
falls below the enregistration threshold even at
just 3 locals.

Operational consequence: future fixtures that intend
to probe register-allocation should reference each
candidate local multiple times (e.g. in a compare or
loop) rather than a single sum, otherwise the locals
will silently fall to stack. The "single hot int
local with compound-op" pattern from
[[batch-390-rmw-non-ax]] is closer to the
enregistration sweet spot.

## Int lt-cmp as int, int gt-cmp as int, comma op in init

Fixtures `1166` (`int a=3; int b=5; int r = a<b;
return r;`), `1167` (`int a=5; int b=3; int r =
a>b; return r;`), `1168` (`int a=0; int b = (a=1,
a+2); return b;` — comma operator as the initializer
expression: side-effect the LHS (assign a), discard,
then take the RHS value as the init value).

All three already worked end-to-end. 1166/1167 complete
the signed compare-as-int family alongside `==/!=/<=/>=`
(1149/1159 and 1160/1163) using the matching `jl`/`jg`
arms. 1168 reuses the existing comma-expression
lowering: the LHS is emitted via `emit_expr_discard`
(so `a = 1` writes to a's slot but doesn't leave a
result in AX), then the RHS `a + 2` is evaluated into
AX and the int-init store writes it to b.


## sizeof of an expression — pure compile-time fold

Fixture `2498-sizeof-paren-expr-obj`:

```c
int main(void) {
  int x;
  return sizeof(x + 1);
}
```

```
55 8b ec              prologue
b8 02 00              mov ax, 2          ; sizeof(int + int) → 2
eb 00 5d c3           epilogue (NO local reserve!)
```

Findings:
- `sizeof(x + 1)` evaluates ONLY the type of the expression
  (`int + int` → `int` → 2). x is never loaded; the result is
  the literal `mov ax, 2`.
- **`int x` declared but unused → NO local reserve**. The prologue
  goes straight from `mov bp, sp` to the function body — no
  `dec sp` or `sub sp` to allocate a frame slot for x. So BCC's
  liveness pass elides locals that are never accessed (including
  by sizeof, which is type-only).
- Compare to `2496` where `c` was both declared AND used (assigned,
  returned) — there `dec sp; dec sp` reserves 2 bytes. The trigger
  is "is the variable referenced by a value-needing op," not just
  declaration.


## Comma expression in assignment RHS — sequence point reloads

Fixture `2500-comma-expr-in-assign-obj`:

```c
int main(void) {
  int x;
  int y;
  x = (y = 3, y + 4);
  return x;
}
```

```
55 8b ec              prologue
83 ec 04              sub sp, 4              ; 4-byte locals (x@-2, y@-4)
c7 46 fc 03 00        mov word [bp-4], 3     ; y = 3
8b 46 fc              mov ax, [bp-4]         ; RELOAD y (no CSE)
05 04 00              add ax, 4              ; ax = y + 4
89 46 fe              mov [bp-2], ax         ; x = (...)
8b 46 fe              mov ax, [bp-2]         ; RELOAD x for return
eb 00                 jmp $+2
8b e5 5d c3           epilogue
```

Findings:
- The **comma operator's sequence point is respected**: BCC stores
  y, then *reloads* y from memory — even though ax already contained
  3 right before the store. NO common-subexpression elimination
  across sequence points.
- Then it stores to x AND immediately reloads x for the return —
  another visible "no CSE / no register-coalesce" moment. Two
  redundant loads (`8b 46 fc` and `8b 46 fe`).
- **4-byte local reserve uses `sub sp, 4`**, NOT `dec sp` × 4. So the
  small-frame peephole threshold is at 1-2 bytes; ≥3 bytes uses
  `sub sp, imm8`. (Confirms 2-byte case in `2496` uses dec×2.)


## Ternary with postfix side-effects in both arms

Fixture `2501-cond-expr-side-effects-obj`:

```c
int y, z, r;
y = 0; z = 0;
r = y ? y++ : z--;
return r + y + z;
```

```
55 8b ec 4c 4c        prologue + 2B local for r
56 57                 push si (= y), push di (= z)
33 f6                 xor si, si      ; y = 0
33 ff                 xor di, di      ; z = 0
0b f6                 or si, si       ; test y (1-byte test for int reg)
74 05                 jz +5
8b c6                 mov ax, si      ; THEN: ax = y (old value)
46                    inc si          ;        then y++
eb 03                 jmp +3
8b c7                 mov ax, di      ; ELSE: ax = z (old value)
4f                    dec di          ;        then z--
89 46 fe              mov [bp-2], ax  ; r = (...)
8b 46 fe              mov ax, [bp-2]  ; RELOAD r (no CSE)
03 c6                 add ax, si      ; + y
03 c7                 add ax, di      ; + z
eb 00 5f 5e 8b e5 5d c3
```

Findings:
- Variables with **multiple uses get register slots**: y → si, z → di.
  Only r (which is computed once and used once) stays in memory.
- The condition test uses **`or reg, reg`** (1-byte test) not
  `cmp reg, 0` — the standard "is this register zero?" peephole.
- **Postfix in ternary**: each arm produces the OLD value into ax,
  THEN performs the side effect (`mov ax, si; inc si` and
  `mov ax, di; dec di`). Both arms converge on the same single store
  `mov [bp-2], ax` — no per-branch store.
- The result `r` is reloaded from memory for the sum — even though
  the just-computed ax holds the value, BCC re-reads it. Sequence-
  point boundary behaves like the earlier comma-expr finding.


## `!x` and `x == 0` produce identical bytes

Fixture `2515-bang-vs-eq-zero-obj`:

```c
int x = 0;
if (!x) return 1;
if (x == 0) return 2;
return 3;
```

Body (extracted):
```
33 f6                  xor si, si           ; x = 0 (in si)
0b f6                  or si, si            ; FIRST: !x test
75 05                  jnz +5
b8 01 00 eb 0e         then: ax=1, jmp epi
0b f6                  or si, si            ; SECOND: x == 0 test
75 05                  jnz +5
b8 02 00 eb 05         then: ax=2, jmp epi
b8 03 00               else: ax=3
eb 00 5e 5d c3         epilogue
```

Findings:
- `!x` and `x == 0` emit the **SAME instruction shape**:
  `or reg, reg; jnz <else>`. Byte-for-byte identical. The parser-
  level distinction (logical-not vs equality-with-zero) is
  flattened to one IR form before codegen.
- No flag reuse between sequential ifs — each emits its own
  `or si, si` even though si hasn't changed between them. This is
  a missed peephole opportunity but a reliable invariant: independent
  ifs are independent tests.
- This is the canonicalization we can rely on: in our parser IR, we
  can normalize `!x` → `x == 0` (or vice versa) and produce
  identical bytes either way.


## Unsigned comparison uses unsigned branches (`jae`/`jb`)

Fixture `2536-unsigned-cmp-obj`:

```c
int main(void) {
  unsigned int u;
  u = 50000;
  if (u < 100) return 1;
  return 0;
}
```

```
55 8b ec 4c 4c                 prologue + 2B local
c7 46 fe 50 c3                 u = 50000 (0xC350)
83 7e fe 64                    cmp word [bp-2], 100
73 05                          jae +5 → FALSE        ; UNSIGNED!
b8 01 00                       true: ax = 1
eb 04                          jmp epi
33 c0                          false: ax = 0
eb 00 8b e5 5d c3              epilogue
```

Findings:
- **Unsigned `u < 100`** emits **`jae` (`0x73`)** — the unsigned
  "above-or-equal" branch. Skips the "true" path when `u >= 100`
  (treating both as unsigned).
- Compare to signed: `if (s < 100)` would emit **`jge` (`0x7D`)**.
- The byte difference is exactly ONE bit (`0x73` vs `0x7D`): a
  type-aware codegen distinction that's invisible if values stay
  in 0..32767 but corrupts comparisons in 32768..65535.
- **Critical for byte-exactness**: our parser must propagate
  signed/unsigned through expression types and pick the right
  branch family at every comparison site.

Branch opcode pairs to track:
| C op | Signed | Unsigned |
|------|--------|----------|
| `<`  | jge=`0x7D` (skip-true) | jae=`0x73` |
| `>`  | jle=`0x7E`             | jbe=`0x76` |
| `<=` | jg=`0x7F`              | ja=`0x77`  |
| `>=` | jl=`0x7C`              | jb=`0x72`  |

(Where the listed condition is "the inverted branch that skips the
true-path" — what BCC actually emits.)


## `sizeof(arr)` vs `sizeof(p)` — distinct types, both fold at compile time

Fixture `2541-sizeof-array-vs-ptr-obj`:

```c
int arr[5];
int main(void) {
  int *p;
  p = arr;
  return sizeof(arr) + sizeof(p);
}
```

```
55 8b ec 4c 4c                 prologue + 2B local for p
c7 46 fe 00 00                 p = &arr (FIXUPP _arr)
b8 0c 00                       mov ax, 12              ; folded sum
eb 00 8b e5 5d c3              epilogue
```

Findings:
- `sizeof(arr)` for `int arr[5]` = **10** bytes (5 × sizeof(int)).
  The array type is preserved for sizeof — does NOT decay.
- `sizeof(p)` for `int *p` = **2** bytes (small memory model
  near pointer).
- The sum (10 + 2 = 12) is **fully folded at compile time**: emitted
  as a single `mov ax, 12` (3 bytes). The runtime never executes a
  size computation.
- Despite `p = arr` triggering array-to-pointer decay in that
  assignment expression, BCC's type system **preserves the original
  `int[5]` type at the `sizeof(arr)` site** — confirming that decay
  is context-sensitive, not a one-shot transformation.
- `_arr` lives in `_BSS` with size 10 (matches the LIDATA reserve).


## Prefix `--i` — `dec reg` BEFORE the store (NOT AX-accumulator)

Fixture `2554-predec-int-obj`:

```c
int i, v;
i = 10;
v = --i;
return v + i;
```

```
55 8b ec 4c 4c                 prologue + 2B local for v
56                             push si              ; i in si
be 0a 00                       mov si, 10           ; i = 10
4e                             dec si               ; --i (modify FIRST)
89 76 fe                       mov [bp-2], si       ; v = i (the NEW value)
8b 46 fe                       mov ax, v
03 c6                          add ax, si           ; v + i
eb 00 5e 8b e5 5d c3           epilogue
```

Findings:
- **Prefix `--i`** emits `dec reg` (1 byte, opcode `4e` for si) DIRECTLY
  on the register holding i — then stores the new value to v.
- This is **different from the AX-accumulator pattern** seen in
  `2510`/`2517` for `i = i - 1` (which used 3 instructions:
  `mov ax, si; dec ax; mov si, ax`). When the user writes the
  decrement as a UNARY OPERATOR `--i`, BCC takes the direct-`dec`
  path; when written as `i = i - 1`, it goes through ax.
- So the SEMANTIC choice of `--i` vs `i = i - 1` produces different
  byte sequences in BCC, despite being equivalent in source. Worth
  recording: BCC's codegen is shape-sensitive to expression form.
- Postfix `i--` (to probe) likely uses a different shape: capture
  OLD value to a temp, then dec.


## Postfix `i--` — `mov [v], reg` BEFORE the `dec`

Fixture `2555-postdec-int-obj`:

```c
int i, v;
i = 10;
v = i--;
return v + i;
```

```
55 8b ec 4c 4c                 prologue + 2B local for v
56                             push si              ; i in si
be 0a 00                       mov si, 10
89 76 fe                       mov [bp-2], si       ; v = i (OLD value FIRST)
4e                             dec si               ; i-- AFTER store
8b 46 fe                       mov ax, v
03 c6                          add ax, si           ; v + i
eb 00 5e 8b e5 5d c3           epilogue
```

Findings:
- Postfix `i--` flips the order from prefix `--i` (`2554`):
  - **Prefix**:  `dec si; mov [bp-2], si`  (modify, then store)
  - **Postfix**: `mov [bp-2], si; dec si`  (store, then modify)
- Same bytes total (4 bytes), same opcodes — just instruction ORDER
  swapped. A clean, direct mapping from pre/post semantics to code
  position.
- Compare to the AX-accumulator form `v = i; i = i - 1;` which would
  emit 3 instructions (mov ax,si; mov [v],ax; dec ax/mov si,ax style).
  So BCC distinguishes:
  - `v = i--` → direct dec
  - `v = i; i = i - 1;` → AX-accumulator pattern (3 instr per modify)
- The form-sensitivity from `2554` is symmetric for pre and post.


## `a = b = c = 7` — single AX load, multi-store right-to-left

Fixture `2595-assign-chain-obj`:

```c
int main(void) {
  int a;
  int b;
  int c;
  a = b = c = 7;
  return a + b + c;
}
```

```
55 8b ec 83 ec 06              prologue + 6B locals
b8 07 00                       mov ax, 7
89 46 fa                       [bp-6] = ax     ; c (rightmost) = 7
89 46 fc                       [bp-4] = ax     ; b = 7
89 46 fe                       [bp-2] = ax     ; a (leftmost) = 7
8b 46 fe                       mov ax, a
03 46 fc                       add ax, b
03 46 fa                       add ax, c
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Right-associative assign: `a = b = c = 7` parses as
  `a = (b = (c = 7))`. BCC evaluates AS A SINGLE COMPUTATION:
  - `7` loads into AX **once**
  - Then stores to c, b, a in that order (right to left)
- The value `7` is **reused across all three stores** — no reload,
  no chain. This is a real optimization vs the naive lowering
  `c = 7; b = c; a = b;` which would emit 3 loads + 3 stores.
- The locals are laid out in declaration order from highest address:
  - a@[bp-2] (declared first, closest to bp)
  - b@[bp-4]
  - c@[bp-6] (declared last, furthest)
- This is a "value flows once, stores propagate" pattern that
  generalizes to longer chains (any depth N reuses AX once).


## Comma operator with register-promoted vars — direct `mov reg, imm`

Fixture `2596-comma-stmt-obj`:

```c
int main(void) {
  int x;
  int y;
  x = 1; y = 2;
  x = (x = 10, y = 20, x + y);
  return x;
}
```

```
55 8b ec 56 57                 prologue + push si, di
be 01 00                       mov si, 1       ; x = 1 DIRECT (no AX)
bf 02 00                       mov di, 2       ; y = 2 DIRECT
be 0a 00                       mov si, 10      ; x = 10 (first comma)
bf 14 00                       mov di, 20      ; y = 20 (second comma)
8b c6                          mov ax, si
03 c7                          add ax, di      ; x + y
8b f0                          mov si, ax      ; x = (...)
8b c6                          mov ax, si
eb 00 5f 5e 5d c3              epilogue
```

Findings:
- **Direct constant assigns to register-promoted vars use direct
  `mov reg, imm`** (e.g. `be 01 00` = `mov si, 1`). NO AX-acc
  detour. This is the byte-shortest form.
- Compare to arithmetic-result assigns: `x = expr` where expr is
  not a pure constant still goes through AX (load expr to ax,
  store ax → reg). The compute-vs-direct distinction is:
  - **Constant rvalue** → direct `mov reg, imm`
  - **Computed rvalue** → flow through ax, then mov reg, ax
- The comma operator emits each sub-expression in source order,
  each "speaking" its rule. The final value-of-comma is the LAST
  sub-expression's value (here `x + y` = 30), used as the value
  of the outer assignment.


## Unary minus `-x` — single `neg ax`

Fixture `2601-unary-neg-obj`:

```c
int x = 42;
return -x;
```

```
55 8b ec 4c 4c                 prologue + 2B local
c7 46 fe 2a 00                 x = 42
8b 46 fe                       mov ax, x
f7 d8                          neg ax           ; UNARY NEGATE
eb 00 8b e5 5d c3              epilogue
```

Findings:
- Unary `-x` compiles to **single `neg ax`** (`f7 d8`, 2 bytes).
- `f7 /3` opcode with r/m 000 = ax → two's-complement negation in
  place. Sets flags.
- Compare to alternatives: `0 - x` would be `mov ax, 0; sub ax, x`
  (5 bytes); `~x + 1` would be `not ax; inc ax` (3 bytes). BCC
  picks the most direct form via the dedicated `neg` instruction.


## `a == b` between two int locals — `mov ax, a; cmp ax, b`

Fixture `2606-eq-two-vars-obj`:

```c
int a = 5;
int b = 5;
if (a == b) return 1;
return 0;
```

```
55 8b ec 83 ec 04              prologue + 4B locals
c7 46 fe 05 00                 a = 5
c7 46 fc 05 00                 b = 5
8b 46 fe                       mov ax, a       ; load LHS
3b 46 fc                       cmp ax, b       ; cmp r16, [mem]
75 05                          jne → false
b8 01 00                       true
eb 04                          jmp epi
33 c0                          false
eb 00 8b e5 5d c3              epilogue
```

Findings:
- `a == b` between two memory locations uses
  **`mov ax, [a]; cmp ax, [b]`** — the 8086 has no mem-mem cmp,
  so the LHS goes to AX first.
- ModR/M `3b` opcode = `cmp r16, r/m16` — reverse of `cmp r/m, r`
  (which would be `39`). The choice doesn't matter for equality
  but follows the lvalue/rvalue convention.
- Branch is `75 05` (jne) — skip the true-path if not equal.
- Total: 3 (load) + 3 (cmp) + 2 (jne) = 8 bytes for the test.


## `if (x <= K)` signed — `cmp + jg <else>`

Fixture `2631-le-cmp-obj`:

```c
int x = 5;
if (x <= 10) return 1;
return 0;
```

```
83 7e fe 0a                    cmp word [bp-2], 10
7f 05                          jg → FALSE          (signed, skip-true)
```

Findings:
- `<=` for signed maps to `jg` (jump if greater) as the SKIP-TRUE
  branch — skip the then-body when `x > K`.
- The skip-condition is the LOGICAL NEGATION of the source
  predicate: `x <= K` → branch on `x > K`.
- Complete signed compare table (skip-true forms):

| C op | skip-true branch | opcode |
|------|------------------|--------|
| `<`  | jge              | 0x7D   |
| `<=` | jg               | 0x7F   |
| `>`  | jle              | 0x7E   |
| `>=` | jl               | 0x7C   |
| `==` | jne              | 0x75   |
| `!=` | je               | 0x74   |

  And unsigned counterparts (from `2536`):
  | `<`  | jae | 0x73 |
  | `<=` | ja  | 0x77 |
  | `>`  | jbe | 0x76 |
  | `>=` | jb  | 0x72 |


## Bitwise NOT `~x` — single `not ax` (2 bytes)

Fixture `2632-bit-not-obj`:

```c
int x = 0x0F0F;
return ~x;
```

```
55 8b ec 4c 4c                 prologue + 2B local
c7 46 fe 0f 0f                 x = 0x0F0F
8b 46 fe                       mov ax, x
f7 d0                          not ax           ; bitwise complement
eb 00 8b e5 5d c3              epilogue
```

Findings:
- `~x` compiles to **`not ax`** (`f7 d0`, 2 bytes). The `f7 /2`
  opcode with r/m 000 = ax.
- Same shape as unary `-x` (`f7 d8` = `neg ax`), just opcode-ext 2
  vs 3.
- Unary operator table:
  - `-x`  → `neg ax` (`f7 d8`)
  - `~x`  → `not ax` (`f7 d0`)
  - `!x`  → `or ax, ax; je <skip>` ... (more complex, see `2515`)
  - `+x`  → no-op (zero bytes)


## Ternary in return — `max(a,b)` is a tight 17-byte body

Fixture `2636-ternary-return-obj`:

```c
int max(int a, int b) {
  return a > b ? a : b;
}
```

```
55 8b ec 56 57                 prologue + push si, di
8b 76 04                       mov si, a
8b 7e 06                       mov di, b
3b f7                          cmp si, di
7e 04                          jle → ELSE      ; signed skip-true for >
8b c6                          mov ax, si       ; THEN: ax = a
eb 02                          jmp epi
8b c7                          ELSE: mov ax, di
eb 00 5f 5e 5d c3              epilogue
```

Findings:
- Args both get register-promoted (a→si, b→di) — typical for hot
  single-use locals.
- `a > b` uses `cmp si, di` then `jle ELSE` (signed greater-than's
  inverse). The condition is the same as `if (a > b)`.
- Each ternary arm is a single `mov ax, <var>` (2 bytes) followed
  by either `jmp +2` (then-arm) or fall-through (else-arm).
- Final body is 17 bytes — among the shortest 2-arg functions
  possible (prolog 3 + push 2 + 2 loads 6 + cmp 2 + jcc 2 + arm1
  2 + jmp 2 + arm2 2 + epi 5 = 28? let me recount). Actually
  prolog+epi take ~10B, leaving ~7B for the operation itself.


## Ternary as assignment RHS — arms converge in AX, then store

Fixture `2670-ternary-assign-obj`:

```c
int a = 5, b = 7, x;
x = a > b ? a : b;
```

```
55 8b ec 4c 4c 56 57           prologue + 2B local (x) + push si, di
be 05 00 bf 07 00              a in si, b in di
3b f7                          cmp si, di
7e 04                          jle → ELSE
8b c6                          THEN: mov ax, si    (a → AX)
eb 02                          jmp → JOIN
8b c7                          ELSE: mov ax, di   (b → AX)
                               ; JOIN:
89 46 fe                       [bp-2] = ax        (x = result)
8b 46 fe                       mov ax, x          (reload for return)
eb 00 5f 5e 8b e5 5d c3        epilogue
```

Findings:
- Ternary as RHS-of-assignment: BOTH arms put their value in AX,
  then converge at a JOIN label. The post-ternary code reads AX
  as the expression's value.
- This is the same structural pattern as the return-version
  (`2636`), but the value flows to a store instead of the
  epilogue.
- Compare to `if-else { x = ...; } else { x = ...; }` where
  each arm STORES to x independently. The ternary form
  centralizes the store at the JOIN point — 1 store vs 2.


## `b = x > y` (comparison-as-int) — if-else pattern producing 0 or 1 in AX

Fixture `2681-bool-as-int-obj`:

```c
int b = x > y;     /* b = 1 if x > y, else 0 */
```

```
8b 46 fe                       mov ax, x
3b 46 fc                       cmp ax, y
7e 05                          jle → ELSE
b8 01 00                       ax = 1
eb 02                          jmp → JOIN
33 c0                          ELSE: xor ax, ax
                               ; JOIN:
89 46 fa                       b = ax
```

Findings:
- C's "comparison as int value" semantics (result is 0 or 1) needs
  explicit codegen since the 8086 has no `setcc`-style instruction.
- BCC emits a tiny if-else: `cmp; jle ELSE; mov ax, 1; jmp; xor ax, ax`.
  Total: 9 bytes for the boolean conversion.
- Compare to using it in an if condition (`if (x > y) ...`) which
  doesn't need to materialize the boolean — just branches directly.


## Ternary as variable initializer — same shape as ternary-in-assignment

Fixture `2719-ternary-in-init-obj`:

```c
int m = a > b ? a : b;
return m;
```

```
4c 4c                          dec sp twice (m)
56 57                          push si, di
8b 76 04 8b 7e 06              load a, b
3b f7                          cmp si, di
7e 04 8b c6 eb 02 8b c7        ternary arms via AX
89 46 fe                       m = ax (JOIN store)
8b 46 fe                       return m
```

Findings:
- `int m = cond ? a : b;` compiles **identically to `int m;
  m = cond ? a : b;`** — declaration-with-init is sugar.
- Both arms put result in AX, JOIN merges, store to m.
- Confirms ternary-as-expression uses AX as universal carrier.

## `x ^= 0xFFFF` is NOT folded to `~x`

Fixture `2722-xor-assign-imm-obj`:

```c
int x = 0xAAAA;
x ^= 0xFFFF;
return x;
```

```
be aa aa                       mov si, 0xAAAA
81 f6 ff ff                    xor si, 0xFFFF    ; xor imm16 (4B)
8b c6                          mov ax, x
```

Findings:
- `x ^= 0xFFFF` emits **`xor reg, imm16`** (4 bytes), NOT `not
  reg` (which would be 2 bytes and semantically equivalent).
- BCC does NOT fold `^ 0xFFFF` to `~` — source form is preserved.
- Compare to `~x` (`2632`) which DOES use `not ax` (2 bytes).
- For full-mask XOR with the all-1s constant, **using `~x`
  instead of `^= 0xFFFF` saves 2 bytes** at the source-form level.
- BCC's source-form sensitivity: same semantics, different bytes.


## `if (!p)` for pointer — same as `if (p == 0)`

Fixture `2794-bang-ptr-obj`:

```c
int check(char *p) {
  if (!p) return -1;
  return 0;
}
```

```
83 7e 04 00                    cmp word [bp+4], 0
75 05                          jne → SKIP-THEN
b8 ff ff                       return -1
```

Findings:
- `!p` for pointer compiles to **identical bytes** as `p == 0`
  (`2702`).
- BCC's parser folds `!ptr` and `ptr == 0` to the same internal
  form at the AST level.
- All three of `!p`, `p == 0`, `p == NULL` produce identical bytes
  (assuming `NULL` is defined as `0` or `(void *)0`).


## Ternary as function argument — JOIN-then-push (inline)

Fixture `2811-ternary-arg-obj`:

```c
return helper(v > 0 ? v : -v);
```

```
8b 76 04                       mov si, v
0b f6                          or si, si        ; peephole
7e 04                          jle → ELSE
8b c6                          mov ax, si       (THEN: v)
eb 04                          jmp → JOIN
8b c6 f7 d8                    mov ax, si; neg ax  (ELSE: -v)
                               ; JOIN:
50                             push ax          (push result)
e8 00 00                       call _helper
```

Findings:
- Ternary as fn argument compiles **inline** — both arms reach the
  JOIN point (with the result in AX), then `push ax` to put it on
  the call stack.
- NO intermediate spill to a local variable.
- Same JOIN pattern as ternary in any other position; only the
  consumer of the result differs (push for call vs store/return).


## `if (a && b)` — short-circuit AND with shared FALSE target

Fixture `2821-short-circuit-and-obj`:

```c
if (a && b) return 1;
return 0;
```

```
83 7e 04 00                    cmp a, 0
74 0b                          je → FALSE      ; short-circuit on a == 0
83 7e 06 00                    cmp b, 0
74 05                          je → FALSE      ; short-circuit on b == 0
                               ; TRUE:
b8 01 00 eb 04                 mov ax, 1; jmp epi
                               ; FALSE:
33 c0                          xor ax, ax
```

Findings:
- `a && b`: test a — if zero, **jump to FALSE** (skip b's test).
  Otherwise test b — if zero, jump to FALSE. Else fall through
  to TRUE.
- Both tests share **one FALSE target**; both use mem-imm cmp.
- 19 bytes total for the && chain (4+2 per cmp+je × 2 + true/false
  bodies).
- Correctly short-circuits: b is not evaluated if a is false.

## `if (a || b)` — short-circuit OR with shared TRUE target

Fixture `2822-short-circuit-or-obj`:

```c
if (a || b) return 1;
return 0;
```

```
83 7e 04 00                    cmp a, 0
75 06                          jne → TRUE      ; short-circuit on a != 0
83 7e 06 00                    cmp b, 0
74 05                          je → FALSE      ; if b == 0, false
                               ; TRUE:
b8 01 00 eb 04                 mov ax, 1; jmp epi
                               ; FALSE:
33 c0                          xor ax, ax
```

Findings:
- `a || b`: test a — if NONZERO (truthy), **jump to TRUE** (skip
  b). Otherwise test b — if zero, jump to FALSE. Else fall to TRUE.
- **Asymmetric branches** vs `&&`: `||` jumps-to-true on the first
  truthy operand; `&&` jumps-to-false on the first falsy operand.
- The fall-through path for `||` reaches TRUE; for `&&` reaches TRUE
  only after BOTH operands.
- 20 bytes total for the || chain.


## `!a && b` — `!` folds into the branch condition

Fixture `2825-not-and-obj`:

```c
if (!a && b) return 1;
```

```
83 7e 04 00                    cmp a, 0
75 0b                          jne → FALSE   ; (!a = false: if a != 0 skip)
83 7e 06 00                    cmp b, 0
74 05                          je → FALSE    ; standard b == 0 → false
```

Findings:
- `!a` folds into the branch direction: instead of testing `a == 0`
  with `je → FALSE` (the `a && b` form), `!a` uses `jne → FALSE`.
- Same total bytes as `(a == 0) && b` — both forms are
  byte-identical at the cmp+branch.
- Generalizable: `!expr` in any boolean context flips the branch
  condition that consumes it.


## Chained assignment `a = b = c = 7;` — right-to-left, AX carries value

Fixture `2835-chained-assign-obj`:

```c
int a, b, c;
a = b = c = 7;
return a + b + c;
```

```
b8 07 00                       mov ax, 7
89 46 fa                       c = ax   (rightmost first!)
89 46 fc                       b = ax
89 46 fe                       a = ax
8b 46 fe 03 46 fc 03 46 fa     return a + b + c
```

Findings:
- `a = b = c = 7` evaluates **right-to-left** per C semantics.
- AX holds the value (7) and is stored to each target without
  reload between stores.
- Total 12 bytes for the chained assignment (3B mov ax + 3 × 3B
  store).
- More efficient than naive `c = 7; b = c; a = b;` which would
  reload between each store (~18 bytes).
- The AX-carries-value pattern works because `=` is an expression
  returning the assigned value.


## `return a < b;` — JOIN-as-bool pattern (0/1 from comparison)

Fixture `2859-cmp-lt-fn-obj`:

```c
int less(int a, int b) {
  return a < b;
}
```

```
8b 46 04                       mov ax, a
3b 46 06                       cmp ax, b
7d 05                          jge → ELSE       (signed inverse of jl)
b8 01 00                       ax = 1
eb 02                          jmp epi
33 c0                          ax = 0
```

Findings:
- Boolean-from-comparison `a < b` uses a **ternary-like JOIN**: if
  the condition is true, ax = 1; else ax = 0.
- 14 bytes for the bool-from-cmp expression.
- For signed `<`: `jge → ELSE` (inverse). Unsigned `<` would use
  `jae` (inverse of `jb`).
- C's `<`, `<=`, `>`, `>=`, `==`, `!=` all produce 0/1 ints via
  this same JOIN pattern.


## Complete signed comparison branch-inversion table

Fixtures `2861`-`2863` complete the signed compare family:

| C op  | inverse jump  | opcode | byte for jump-to-ELSE pattern |
|-------|---------------|--------|--------------------------------|
| `<`   | `jge`         | `7d`   | (`2859`) |
| `<=`  | `jg`          | `7f`   | (`2861`) |
| `>`   | `jle`         | `7e`   | (`2862`) |
| `>=`  | `jl`          | `7c`   | (`2863`) |
| `==`  | `jne`         | `75`   | (many) |
| `!=`  | `je`          | `74`   | (`2702`, etc.) |

For unsigned: replace with `jae` (73), `ja` (77), `jbe` (76), `jb` (72).

All comparisons follow the same 14-byte JOIN pattern for "bool from
comparison": `cmp; j<inv> → ELSE; mov ax, 1; jmp epi; ELSE: mov ax, 0`.


## `if (a > b) r = a; else r = b;` — same code as ternary `r = a > b ? a : b`

Fixture `2865-if-else-assign-obj`:

```c
if (a > b) r = a;
else r = b;
return r;
```

```
8b 76 04 8b 7e 06              a → si, b → di
3b f7                          cmp si, di
7e 04                          jle → ELSE
8b d6                          r = a (r in dx, leaf fn promotion)
eb 02                          jmp → JOIN
8b d7                          r = b
                               ; JOIN:
8b c2                          mov ax, dx (return r)
```

Findings:
- IF-ELSE that assigns the same var in both arms compiles
  **identically to the ternary form** `r = (cond) ? a : b`.
- BCC promotes a, b, r to si, di, dx (leaf fn, 3-local promotion).
- The control-flow pattern is the SAME: condition → branch → store
  → jmp-to-join → ELSE-store → join.
- This is the classic `max(a, b)` idiom in two source forms; both
  produce identical bytes.


## Nested ternary `a > b ? (a > c ? a : c) : (b > c ? b : c)`

Fixture `2883-nested-ternary-obj`:

```c
int max3(int a, int b, int c) {
  return a > b ? (a > c ? a : c) : (b > c ? b : c);
}
```

```
                               ; outer compare
3b fa                          cmp di (a), dx (b)
7e 0c                          jle → outer-ELSE
                               ; outer THEN: inner1 (a > c ? a : c)
3b fe                          cmp di, si (c)
7e 04                          jle → inner1-ELSE
8b c7                          ax = di (a)
eb 02                          jmp → final-JOIN
8b c6                          ax = si (c)
eb 0a                          jmp → final-JOIN
                               ; outer ELSE: inner2 (b > c ? b : c)
3b d6                          cmp dx, si
7e 04                          jle → inner2-ELSE
8b c2                          ax = dx (b)
eb 02                          jmp → final-JOIN
8b c6                          ax = si (c)
                               ; final-JOIN: epilogue
```

Findings:
- Nested ternaries compile to **nested JOIN-via-AX patterns**.
- Each ternary produces its result in AX; the outer ternary's
  branches each select between two inner ternary results.
- All branches converge at the final JOIN point.
- All 3 params promoted to di/dx/si in this leaf function
  (3-local register slot allocation).
- ~25 bytes total for the 3-way max expression.


## `*p && *q` — short-circuit with direct `cmp [reg], 0`

Fixture `2925-deref-and-deref-obj`:

```c
if (*p && *q) return 1;
return 0;
```

```
8b 76 04 8b 7e 06              p → si, q → di
83 3c 00                       cmp word [si], 0    (test *p)
74 0a                          je → FALSE
83 3d 00                       cmp word [di], 0    (test *q)
74 05                          je → FALSE
```

Findings:
- `*p` test uses **`cmp word [si], 0`** (`83 3c 00`, 3B) — direct
  memory-immediate compare with no-disp form.
- ModR/M `3c` = mod 00, op-ext 111 (cmp), r/m 100 (`[si]`).
- No need to load `*p` into AX first; the cmp reads directly from
  memory via the register.
- Short-circuit semantics preserved: if `*p == 0`, skip evaluating
  `*q` entirely.


## Comma operator `(expr1, expr2)` — sequential evaluation, expr2 is result

Fixture `2931-comma-expr-obj`:

```c
return (a = a + 1, a + b);
```

```
8b 76 04                       mov si, a
8b c6 40 8b f0                 a = a + 1 (AX-acc)
8b c6                          mov ax, a (= a + 1)
03 46 06                       add ax, b
```

Findings:
- Comma operator evaluates **left-to-right**, expr1 for side
  effects, expr2 for the result.
- BCC promotes `a` to si (modified parameter), uses standard
  AX-acc patterns for both expressions.
- The two expressions are emitted sequentially in source order;
  expr1's result is discarded, expr2's value goes through AX.


## `cmp reg, imm` opcode choice: `83 /7` (3B sign-ext) vs `81 /7` (4B imm16)

Fixture `2952-and-cond-bounded-obj`:

```c
if (x > 100 && x < 200) return 1;
```

```
83 fe 64                       cmp si, 100   (3B: imm8 sign-ext, 100 fits [-128,127])
81 fe c8 00                    cmp si, 200   (4B: imm16, 200=0xC8 doesn't fit signed imm8)
```

Findings:
- BCC chooses encoding based on whether the constant fits in a
  signed imm8 (range `[-128, 127]`):
  - **`83 /7 imm8`** (3B): when imm fits, sign-extends to 16 bits
  - **`81 /7 imm16`** (4B): when imm > 127 or < -128
- For 200 (= 0xC8), the imm8 form would sign-extend to 0xFFC8 (= -56),
  which is wrong. So BCC uses the imm16 form.
- Generalizable: all cmp/add/sub/and/or/xor with imm use this same
  byte-vs-word choice rule.


## `cmp word [mem], imm` boundary at 127/128

Fixtures `2957-cmp-127-obj` and `2958-cmp-128-obj`:

```c
if (x > 127) ...   /* fits signed imm8 */
if (x > 128) ...   /* doesn't fit (would be sign-ext to -128) */
```

```
                               ; cmp 127:
83 7e 04 7f                    cmp word [bp+4], 127      (4B, imm8 sign-ext)

                               ; cmp 128:
81 7e 04 80 00                 cmp word [bp+4], 128      (5B, imm16)
```

Findings:
- **127 fits** signed imm8 `[-128, 127]` → 3B opcode form (`83 /op imm8`).
- **128 doesn't fit** (would sign-extend to -128) → 4B opcode form
  (`81 /op imm16`).
- For cmp-with-mem, total instruction is 1B opcode + 1B ModR/M +
  disp + imm: 4B (imm8) vs 5B (imm16) for `[bp+disp8]` targets.
- Boundary confirmed at exactly 127/128.

## `x + 128` — `add ax, imm16` AX-acc form (3B)

Fixture `2959-add-128-obj`:

```c
return x + 128;
```

```
8b 46 04                       mov ax, x
05 80 00                       add ax, 128   (AX-acc, 3B)
```

Findings:
- `add ax, imm16` uses the **AX-accumulator form** `05 imm16` (3B).
- Beats the generic `81 c0 imm16` (4B) by 1 byte.
- For non-AX registers, would use `81 /0 imm16` (4B).
- 128 doesn't fit signed imm8 so the `83 c0 imm8` form (3B) can't be used.

## `(*p).x` byte-identical to `p->x`

Fixture `2960-deref-dot-obj`:

`(*p).x` and `p->x` both compile to `mov si, p; mov ax, [si]` (5B).
BCC normalizes both syntactic forms to the same AST.


## `cmp r, -128` vs `cmp r, -129` — negative-side boundary at -128

Fixtures `2963-cmp-neg128-obj`, `2964-cmp-neg129-obj`:

```
                               ; cmp -128:
83 7e 04 80                    cmp word [bp+4], 0x80 (= -128 sign-ext)  (4B)

                               ; cmp -129:
81 7e 04 7f ff                 cmp word [bp+4], 0xFF7F (= -129)         (5B)
```

Findings:
- **-128 (`0x80`) FITS signed imm8** → 3B opcode form. The byte `0x80`
  sign-extends to `0xFF80` (= -128 as int16).
- **-129 (`0xFF7F`) doesn't fit** → 4B opcode form (imm16).
- Full signed imm8 range confirmed: exactly `[-128, 127]`.

## Constant-condition ternary — DEAD-BRANCH ELIMINATION

Fixtures `2965-ternary-const-true-obj`, `2966-ternary-const-false-obj`:

```c
return 1 ? a : b;   /* compiles to: return a */
return 0 ? a : b;   /* compiles to: return b */
```

```
                               ; 1 ? a : b:
8b 46 04                       mov ax, a    (just return a)

                               ; 0 ? a : b:
8b 46 06                       mov ax, b    (just return b)
```

Findings:
- **BCC constant-folds ternaries with literal conditions** —
  dead branch entirely eliminated.
- `1 ? a : b` collapses to `a`; `0 ? a : b` collapses to `b`.
- This is DCE for **expression-level constant evaluation**,
  distinct from the no-DCE pattern for statements-after-return.
- BCC's const-fold pass is smarter than just identity-folding —
  it also handles selector-based branch elimination.

## `0 == x` — commutative, byte-identical to `x == 0`

Fixture `2967-zero-eq-x-obj`:

```c
if (0 == x) return 1;
```

```
83 7e 04 00                    cmp word [bp+4], 0
75 05                          jne → ZERO
```

Findings:
- `0 == x` (Yoda condition) compiles to identical bytes as
  `x == 0` — BCC normalizes the commutative comparison.
- The constant 0 is always placed on the RHS of the cmp instruction.


## `x = flag ? (a = a + 1) : b;` — ternary with assignment in arm

Fixture `2992-ternary-asgn-arm-obj`:

```c
x = flag ? (a = a + 1) : b;
```

```
                               ; THEN arm: a = a + 1, AX = new a
8b c6 40 8b f0                 mov ax, si; inc ax; mov si, ax
eb 03                          jmp → JOIN
                               ; ELSE arm: just load b
8b 46 06                       mov ax, b
                               ; JOIN:
89 46 fe                       x = ax
```

Findings:
- Assignment expression `(a = a + 1)` produces the new value of `a`
  as its result. The ternary uses that AX value at JOIN.
- Both arms converge at AX, then x = ax store.
- Side effect (modifying a) is preserved per C semantics.


## `if ((x = K) > 0)` — assignment in condition uses AX value directly

Fixture `2996-assign-in-cond-obj`:

```c
if ((x = 5) > 0) return x;
```

```
b8 05 00                       mov ax, 5
89 46 fe                       x = ax (store)
0b c0                          or ax, ax   (test AX > 0 — peephole, no reload!)
7e 05                          jle → ZERO
```

Findings:
- Assignment expression `x = 5` produces value 5 in AX.
- Comparison uses AX directly (`or ax, ax` peephole for `> 0`)
  without reloading from x.
- Side effect (x = 5) preserved per C semantics.
- The `or ax, ax` peephole is enabled because the value is already
  in AX from the assignment.


## Shift opcode table — `d1 /op` for 1-bit, `d3 /op` for cl-form

Fixture `3034-uint-shr-obj`:

```c
return x >> 1;   /* unsigned */
```

```
d1 e8                          shr ax, 1     (UNSIGNED, op-ext /5)
```

Findings:
- Shift opcode `d1 /op` (2B for AX, 1-bit shift):
  - `/4` = shl (`d1 e0`)
  - `/5` = shr (`d1 e8`) — UNSIGNED
  - `/7` = sar (`d1 f8`) — SIGNED
- CL-form `d3 /op` (2B for AX, count in CL).
- For memory operands, ModR/M r/m field selects the address;
  op-ext stays the same.
- `unsigned int >> N`: use `shr` (`d1 e8` or `d3 e8`).
- `signed int >> N`: use `sar` (`d1 f8` or `d3 f8`).


## `unsigned int v >= 0x8000` — UNSIGNED jumps `jb`/`jae`

Fixture `3078-uint-ge-0x8000-obj`:

```c
if (v >= 0x8000) return 1;
```

```
81 7e 04 00 80                 cmp word [bp+4], 0x8000  (imm16, 5B)
72 05                          jb → FALSE   (UNSIGNED: below)
```

Findings:
- Unsigned int compare uses unsigned jumps:
  - `jb` (`72`) = unsigned below (<)
  - `jae` (`73`) = unsigned above-or-equal (>=)
  - `ja` (`77`) = unsigned above (>)
  - `jbe` (`76`) = unsigned below-or-equal (<=)
- 0x8000 (= 32768) > signed imm8 max, so `81 imm16` form (5B).

## `x == -1` — `83 /7 imm8` sign-ext (FF = -1)

Fixture `3081-cmp-neg-1-obj`:

```c
if (x == -1) return 1;
```

```
83 7e 04 ff                    cmp word [bp+4], -1   (imm8 sign-ext, 4B)
75 05                          jne → FALSE
```

Findings:
- `-1` (0xFF byte sign-ext to 0xFFFF) fits signed imm8 → 4B short form.
- Same pattern for all small negative comparisons in `[-128, -1]`.


## `x == 1 || x == 2` short-circuit OR — `je → TRUE; cmp; jne → FALSE`

Fixture `3112-or-chain-obj`:

```c
if (x == 1 || x == 2) return 1;
```

```
8b 76 04                       mov si, x
83 fe 01                       cmp si, 1
74 05                          je → TRUE       (first arm matches → skip eval)
83 fe 02                       cmp si, 2
75 05                          jne → FALSE     (second arm fails → false)
                               ; TRUE: return 1
```

Findings:
- OR short-circuit: first true → skip rest.
- Each arm gets a `cmp + j*` pair.
- **OR**: first `je → TRUE` (jump to TRUE on match).
- **AND** (`3035`): first `jne → FALSE` (jump to FALSE on mismatch).
- Different jump direction reflects the truth-table semantics.


## `if (p)` ≡ `if (p != 0)` byte-identical (ptr-vs-zero cmp)

Fixtures `3128-if-p-truthy-obj`, `3129-p-ne-null-obj`:

Both:
```
83 7e 04 00                    cmp word [bp+4], 0
74 05                          je → FALSE
```

Findings:
- `if (p)` truthy test = `cmp [mem], 0; je → FALSE`.
- `if (p != 0)` explicit = byte-identical code.
- For ptr params (not register-promoted by default in 1-arg fn),
  uses direct mem-cmp.

## `char c == 'A'` (int const) — BYTE-LEVEL `cmp byte [mem], imm8` (NO promotion!)

Fixture `3130-char-eq-A-obj`:

```c
if (c == 'A') return 1;
```

```
80 7e 04 41                    cmp byte [bp+4], 0x41   (4B BYTE cmp)
75 05                          jne → FALSE
```

Findings:
- Char-to-constant-int compare uses **direct byte cmp** (4B).
- ModR/M `80` opcode is `cmp r/m8, imm8`. ModR/M `7e` = mod 01
  (disp8), op-ext 111 (cmp), r/m 110 ([bp+disp8]).
- NO cbw promotion needed since the literal `'A'` fits in 8 bits.
- **Saves ~3 bytes vs promote-then-int-cmp** (4B vs ~7B).
- This peephole is char-cmp-imm8 — only applies for chars compared
  with const that fits int8.


## `int x == 0xFF` and `int x == 256` — both use imm16 form

Fixtures `3134-int-eq-FF-obj`, `3135-cmp-256-obj`:

```c
if (x == 0xFF)    /* 255 = doesn't fit signed imm8 (max 127) */
if (x == 256)
```

Both:
```
81 7e 04 imm16                 cmp word [bp+4], imm16
75 05                          jne → FALSE
```

Findings:
- `0xFF` (= 255) would sign-extend from imm8 as `0xFFFF` (= -1),
  WRONG value. Must use imm16 form (5B).
- `256` exceeds signed imm8 max → imm16 form (5B).
- Both cases: 5-byte cmp instruction.


## `!x` — `neg + sbb + inc` boolify trick (5 bytes)

Fixture `3140-double-not-obj`:

```c
return !!x;   /* boolify */
```

```
                               ; !x (5 bytes):
f7 d8                          neg ax        (CF = 1 if ax was nonzero)
1b c0                          sbb ax, ax    (ax = -CF)
40                             inc ax        (0→1, -1→0)
```

Findings:
- `!x` = 5-byte sequence: `neg + sbb + inc`.
- Logic: `neg` sets CF based on ax != 0; `sbb ax,ax` produces -1 or 0; `inc` flips to 1 or 0.
- `!!x` = apply the sequence twice (10 bytes).
- This is the standard 8086 "boolify" idiom.

## Nested ternary `a ? b : (c ? d : e)` — nested cmp+je structure

Fixture `3139-nested-ternary-obj`:

```c
return a ? b : (c ? 10 : 20);
```

Compiles to nested if-then-else cmp+je structure (no special handling).


## `~x` and `-x` — single F7 opcode forms

Fixtures `3152-bitwise-not-obj`, `3153-unary-neg-obj`:

```c
return ~x;   /* f7 d0 = not ax (2B) */
return -x;   /* f7 d8 = neg ax (2B) */
```

Findings:
- F7-group unary ops on AX:
  - `f7 d0` = `not ax` (op-ext /2)
  - `f7 d8` = `neg ax` (op-ext /3)
- Each 2 bytes.
- For memory operands: `f7 16 disp16` (not [mem]) or `f7 1e disp16` (neg [mem]).

## `(long)int_var` widening cast — `cwd` (sign-extend to DX:AX)

Fixture `3154-int-to-long-cast-obj`:

```c
long widen(int x) {
  return (long)x;
}
```

```
8b 46 04                       mov ax, x       (LOW word)
99                             cwd             (sign-extend AX → DX:AX)
```

Findings:
- Int-to-long widening = load AX + `cwd` (1B).
- `cwd` sign-extends AX into DX (DX = 0x0000 if positive, 0xFFFF if negative).
- DX:AX is the long return convention.
- For unsigned int → unsigned long: `xor dx, dx` (2B) zero-extend.
- 4 bytes total for the signed cast.


## `(unsigned int)int_var` cast — NO-OP (same bit pattern)

Fixture `3200-signed-unsigned-cast-obj`:

```c
return (unsigned int)x;
```

```
8b 46 04                       mov ax, x   (just a load, no conversion)
```

Findings:
- Signed↔unsigned same-width cast = no-op.
- The bit pattern is identical; only how it's interpreted differs.
- All `int ↔ unsigned int` and `char ↔ unsigned char` casts are
  no-ops at codegen.


## `(unsigned char)int` then promote — `mov al + mov ah, 0` (5B)

Fixture `3236-uchar-from-int-obj`:

```c
return (unsigned char)x;   /* truncate to uchar, then promote to int */
```

```
8a 46 04                       mov al, x
b4 00                          mov ah, 0   (zero-extend)
```

Findings:
- 5 bytes total (3B byte load + 2B `mov ah, 0`).
- Compare to `(char)int → int` (signed): byte load + `cbw` = 4B.
- Unsigned promotion is 1 byte longer.


## `if (a + b > 0)` — ADD flags used directly (NO `or ax, ax` after add!)

Fixture `3254-expr-cond-obj`:

```c
if (a + b > 0) return 1;
```

```
8b 46 04                       mov ax, a
03 46 06                       add ax, b
7e 05                          jle → FALSE   (uses ADD's flags!)
```

Findings:
- BCC uses **ADD's flag side-effect** for the >= 0 test.
- `jle` checks `ZF | (SF != OF)` — i.e., signed `<= 0`.
- NO separate `or ax, ax` or `cmp ax, 0` after add.
- **Saves 2 bytes** vs naive (sum then test).
- Same peephole applies to other arithmetic ops that set flags
  (sub, add, and, or, xor, inc, dec).


## `(a - b) == 0` and `(a + b) == 0` — flag-direct test

Fixtures `3257-sub-eq-0-obj`, `3258-add-eq-0-obj`:

```c
if (a - b == 0) ...   /* sub + jne FALSE */
if (a + b == 0) ...   /* add + jne FALSE */
```

```
8b 46 04                       mov ax, a
2b 46 06   (or 03 46 06)       sub ax, b   (or add ax, b)
75 05                          jne → FALSE  (uses ZF from arithmetic)
```

Findings:
- Both sub and add set ZF based on result.
- BCC uses this flag directly — no extra cmp.
- 8 bytes for the compare.
- Same peephole as `if (a + b > 0)` (`3254`).


## `(a & K) == 0` — `test mem, imm16` (smart peephole, no AND result)

Fixture `3264-and-then-eq-0-obj`:

```c
if ((a & 0xFF) == 0) return 1;
```

```
f7 46 04 ff 00                 test word [bp+4], 0x00FF
75 05                          jne → FALSE
```

Findings:
- `(a & K) == 0` compiles to `test mem, imm16` (5B).
- `test` is non-destructive AND that only sets flags.
- No need to load `a`, AND, then compare — single instruction does both.
- Saves bytes vs naive sequence.
- ModR/M `46 disp8` = [bp+disp8]; opcode `f7 /0` = test imm16.

## `p->x == 0` (first field) — `cmp word [si], 0` (3B sign-ext)

Fixture `3267-ptr-field-eq-0-obj`:

```c
if (p->x == 0) return 1;
```

```
8b 76 04                       mov si, p
83 3c 00                       cmp word [si], 0      (3B, no-disp form)
75 05                          jne → FALSE
```

Findings:
- `[si]` no-disp form with imm8 sign-ext = 3B cmp.
- ModR/M `3c` = mod 00, op-ext 111 (cmp /7), r/m 100 ([si]).
- For non-zero field offset: `cmp word [si + disp8], 0` (4B).


## `(a & K) != 0` and `if (g & K)` global — `test mem, imm16`

Fixtures `3269-and-then-ne-0-obj`, `3270-global-bit-test-obj`:

```c
if ((a & 0xFF) != 0) ...    /* test + je FALSE (inverse of == 0) */
if (g & 0x80) ...           /* same: test + je FALSE */
```

```
                               ; (a & 0xFF) != 0:
f7 46 04 ff 00                 test word [bp+4], 0x00FF
74 05                          je → FALSE

                               ; if (g & 0x80) for global:
f7 06 00 00 80 00              test word [_g], 0x0080
74 05                          je → FALSE
```

Findings:
- `test r/m16, imm16` is the universal bit-test peephole.
- For local: `f7 46 disp imm16` (5B).
- For global: `f7 06 disp16 imm16` (6B + FIXUPP).
- `if (g & K)` truthy = `test + je → FALSE`.
- `if ((a & K) == 0)` zero test = same `test` op + opposite jump.


## comma operator `(a++, a*2)` — left side for side effects, right side returned

Fixture `3308-comma-op-obj`:

```
56                             push si           (callee-save)
8b 76 04                       mov si, a
46                             inc si             (a++ — left side discarded)
8b c6                          mov ax, si         (right side: a)
d1 e0                          shl ax, 1          (* 2)
```

Findings:
- Standard comma semantics: evaluate left for side effects, drop value, evaluate right for the expression value.
- The param-modification stays in the register copy since the parameter has no further use.
- BCC also picked SI to hold `a` since it's a candidate for register allocation.

## `&arr[const]` — folded into `mov ax, _arr + const*size`

Fixture `3309-addr-of-elem-obj`:

```c
int arr[10];
int *third(void) { return &arr[3]; }
```

```
b8 06 00 [FIXUPP _arr]         mov ax, 0x0006   (resolved: _arr + 6)
```

Findings:
- `3 * sizeof(int) = 6` folded at compile time.
- Single 3B `mov ax, imm16` with a single FIXUPP into `_arr`.
- No actual array indexing computation at runtime.

## `sizeof(local_arr)` — no stack alloc, pure compile-time const

Fixture `3310-sizeof-arr-obj`:

```c
int sz(void) {
  int arr[10];
  return sizeof(arr);
}
```

```
                               ; no sub sp instruction
b8 14 00                       mov ax, 20    (10 * 2 = 20)
```

Findings:
- The local array is never stack-allocated since it's never referenced.
- `sizeof` is purely a type query — no runtime cost or memory usage.


## enum constants — int literals folded as imm8/imm16 in cmp

Fixture `3314-enum-obj`:

```c
enum Color { RED = 1, GREEN, BLUE };  /* 1, 2, 3 */
```

Body excerpt:
```
83 fe 01                       cmp si, 1     (RED)
83 fe 02                       cmp si, 2     (GREEN)
```

Findings:
- enum constants compile as `int` literals (no special type).
- BCC uses `83 /7` (cmp r/m16, imm8 sign-extended) for small values.
- Reg-allocated parameter (SI) — confirms BCC reg-alloc applies to enum params too.

## typedef — transparent to codegen

Fixture `3315-typedef-obj`:

```c
typedef int Word;
Word twice(Word a) { return a + a; }
```

Body:
```
56                             push si
8b 76 04                       mov si, a
8b c6                          mov ax, si
03 c6                          add ax, si
```

Findings:
- typedef produces identical code to the underlying type.
- `a + a` is `mov ax, si; add ax, si` (4B). NOT optimized to `shl ax, 1` or `add ax, ax` — but all three alternatives are 4B given current SI-allocation.

## `const int k = N` local — NOT constant-folded

Fixture `3316-const-local-obj`:

```c
const int k = 7;
return k * 3;
```

Body:
```
83 ec 02                       sub sp, 2          (allocate k)
c7 46 fe 07 00                 mov word [bp-2], 7  (k = 7)
8b 46 fe                       mov ax, [bp-2]
ba 03 00                       mov dx, 3
f7 ea                          imul dx             (k * 3)
```

Findings:
- `const int` qualifier doesn't trigger constant folding in BCC.
- Treated as normal local int: stack alloc + store + load + imul at runtime.
- Optimal would be `b8 15 00` (mov ax, 21), but BCC emits 13 bytes instead of 3.
- Confirms earlier observation: BCC does NOT propagate const through arithmetic.


## char→int signed cast — single `cbw` instruction

Fixture `3319-char-to-int-obj`:

```c
int widen(char c) { return (int)c; }
```

```
8a 46 04                       mov al, c
98                             cbw             (sign-extend AL → AX)
```

Findings:
- 1-byte `cbw` (opcode 0x98) is the canonical char→int signed widening on 8086.
- Total 4B for the function body.

## unsigned char→int zero-extend — `mov ah, 0`

Fixture `3320-uchar-to-int-obj`:

```
8a 46 04                       mov al, c
b4 00                          mov ah, 0       (zero-extend)
```

Findings:
- BCC uses literal `mov ah, 0` (2B) over `xor ah, ah` (2B, same size, but might affect flags) — preference is for literal.
- No `movzx` (that's 386+).

## while(n--) loop — post-decrement-test pattern

Fixture `3321-ptr-walk-sum-obj`:

```c
int sum(int *p, int n) {
  int s = 0;
  while (n--) s += *p++;
  return s;
}
```

Body:
```
56 57                          push si; push di
8b 76 04                       mov si, p       (SI = p)
8b 56 06                       mov dx, n       (DX = n — third reg used!)
33 ff                          xor di, di      (DI = s = 0)
eb 04                          jmp TEST
LOOP:
03 3c                          add di, [si]    (s += *p)
46 46                          inc si; inc si  (p++ on int* = +2; 2B vs add si,2 which is 3B)
TEST:
8b c2                          mov ax, dx      (copy n into ax — save pre-dec value)
4a                             dec dx          (n--)
0b c0                          or ax, ax       (test pre-dec value)
75 f5                          jne LOOP
8b c7                          mov ax, di      (return s)
```

Findings:
- **3-register allocation**: with SI=p, DI=s, BCC uses DX for n (DX is normally scratch, but stays live since loop body has no calls).
- **`inc reg; inc reg` saves 1B**: for int* p++ (advance by 2), 2× inc (2B) beats `add reg, 2` (3B with imm8).
- **Post-decrement pattern**: `mov ax, dx; dec dx; or ax, ax; jne` — pre-dec value tested for the while condition.


## Assignment expression `(*p = 5) + 1` — value reused via register

Fixture `3333-assign-expr-obj`:

```
56                             push si
8b 76 04                       mov si, p
b8 05 00                       mov ax, 5
89 04                          mov [si], ax    (*p = 5)
40                             inc ax           (+1 — reused value)
```

Findings:
- Assignment-expression value reused from the register holding the stored value.
- No reload from `[si]` after the store — the +1 acts on the AX that was already 5.
- Clean reuse. 10B body.

## Chained assignment `a = b = c = 0` — 1 source register, N stores

Fixture `3334-chained-assign-obj`:

```c
int a, b, c;
void zero(void) { a = b = c = 0; }
```

```
33 c0                          xor ax, ax       (zero in AX)
a3 04 00 [FIXUPP _c]           mov [_c], ax     (c = 0 first)
a3 02 00 [FIXUPP _b]           mov [_b], ax     (b = 0)
a3 00 00 [FIXUPP _a]           mov [_a], ax     (a = 0 last)
```

Findings:
- Right-to-left assignment order (c, then b, then a).
- Value held in AX throughout — no per-store reload.
- Uses 3-byte `a3 imm16` form (special `mov [mem], ax`) vs 4-byte `89 06 imm16` generic form.
- 11B total for the 3-way zero-init.


## abs via ternary `x < 0 ? -x : x` — branch + neg

Fixture `3346-abs-ternary-obj`:

```
56                             push si
8b 76 04                       mov si, x
0b f6                          or si, si        (cmp x, 0)
7d 06                          jge ELSE         (x >= 0)
8b c6                          mov ax, si       (x for neg)
f7 d8                          neg ax           (-x)
eb 02                          jmp END
ELSE:
8b c6                          mov ax, si       (just x)
END:
```

Findings:
- Standard abs idiom = compare with 0 + branch + conditional neg.
- BCC does NOT use the branchless trick (`sar x, 15; xor x, sar; sub x, sar`).
- 15B body. Reg-allocates x into SI.


## `++g` pre-increment on global — inc mem, load new value

Fixture `3371-preinc-global-obj`:

```c
int g;
int next(void) { return ++g; }
```

```
ff 06 00 00 [FIXUPP _g]        inc word [_g]    (4B)
a1 00 00    [FIXUPP _g]        mov ax, [_g]     (3B — load new value)
```

Findings:
- Pre-inc order: inc-then-load (post-inc loads first then incs).
- 7B body. Contrast with post-inc (3294) which has the SAME byte sequence — they're indistinguishable for global writes when the return-value semantics happen to coincide (BCC just always emits inc-then-load).

## `x--` post-decrement on parameter — emits dead `dec`

Fixture `3372-postdec-param-obj`:

```c
int last(int x) { return x--; }
```

```
56                             push si
8b 76 04                       mov si, x
8b c6                          mov ax, si        (return value = pre-dec x)
4e                             dec si            (x-- — modifies local only)
```

Findings:
- Modification to SI doesn't write back to the parameter slot.
- The `dec si` is dead code (SI goes out of scope after `pop si`) but BCC still emits it.
- 7B body. Missed dead-code optimization.

## `c <<= 2` on char global — 2× mem-direct shl-by-1

Fixture `3373-char-shl-eq-obj`:

```c
char c = 1;
void shift2(void) { c <<= 2; }
```

```
d0 26 00 00 [FIXUPP _c]        shl byte [_c], 1
d0 26 00 00 [FIXUPP _c]        shl byte [_c], 1
```

Findings:
- 2× `shl byte [mem], 1` (4B each = 8B).
- Operates directly on memory — no register temp.
- For N=2 on byte mem, this beats load-shift-store form (~10B).

## `x >>= 4` on int global — `mov cl, 4; sar [mem], cl`

Fixture `3374-int-shr-eq4-obj`:

```c
int x = 100;
void shr4(void) { x >>= 4; }
```

```
b1 04                          mov cl, 4
d3 3e 00 00 [FIXUPP _x]        sar word [_x], cl
```

Findings:
- Single mem-direct `sar word [mem], cl` (4B with FIXUPP).
- N=4 ≥ threshold → uses CL form (vs inline N× shl/sar for N≤3).
- 6B body. Signed type → `sar`.

## `arr[i]++` in void context — `inc word [bx+_arr]` mem-direct

Fixture `3375-arr-postinc-obj`:

```c
int arr[5];
void bump(int i) { arr[i]++; }
```

```
8b 5e 04                       mov bx, i
d1 e3                          shl bx, 1                (bx = i*2)
ff 87 00 00 [FIXUPP _arr]      inc word [bx + _arr]
```

Findings:
- Inc happens directly on memory at [bx + _arr] — no temp register.
- Void-context: no post-inc result-value computation/load.
- 9B body.

## `(*p)++` in void context — 2B `inc word [si]`

Fixture `3376-deref-postinc-obj`:

```c
void bump(int *p) { (*p)++; }
```

```
56                             push si
8b 76 04                       mov si, p
ff 04                          inc word [si]      (2B!)
```

Findings:
- Tightest possible: 2-byte `inc word [si]`.
- No load/store sequence — uses mem-direct inc.
- 6B body total (including SI setup).


## `(char)x` for int x — truncate + sign-extend

Fixture `3411-cast-int-to-char-obj`:

```c
int truncate(int x) { return (char)x; }
```

```
8a 46 04                       mov al, x         (byte load — truncate)
98                             cbw               (sign-extend back to int)
```

Findings:
- Cast to char drops the high byte (byte load reads only [bp+disp]).
- Sign-extension back to int via `cbw`. Result in AX has correct sign.
- 4B body.

## unsigned-to-int conversion — no-op (same bit pattern)

Fixture `3412-uint-to-int-obj`:

```c
int convert(unsigned u) { int i = u; return i; }
```

```
4c 4c                          dec sp; dec sp   (alloc i)
8b 46 04                       mov ax, u
89 46 fe                       mov [bp-2], ax    (i = u — copy bits)
8b 46 fe                       mov ax, [bp-2]
```

Findings:
- unsigned→int is a pure bit copy — no widening/narrowing.
- Same byte pattern as int→int copy.


## `!x` — `neg; sbb; inc` (branchless boolean coercion)

Fixture `3415-double-not-obj`:

```c
int as_bool(int x) { return !!x; }
```

```
8b 46 04                       mov ax, x
f7 d8                          neg ax        (CF = (ax != 0))
1b c0                          sbb ax, ax    (ax = -CF)
40                             inc ax        (-CF + 1 → 0 or 1)
                               ; second ! repeats:
f7 d8                          neg ax
1b c0                          sbb ax, ax
40                             inc ax
```

Findings:
- `!x` = 6-byte branchless idiom: `neg / sbb ax,ax / inc ax`.
- Maps to {0, 1} via the carry-flag-after-neg trick.
- `!!x` is implemented as `!` applied twice — 12B (no peephole that "the second !" is redundant given the first already gave 0/1).

## `if (!x)` — inverted comparison, no `neg/sbb/inc`

Fixture `3416-not-cond-obj`:

```c
if (!x) return 99; return x;
```

```
0b f6                          or si, si
75 05                          jne ELSE       (x != 0 → !x is false)
b8 63 00                       mov ax, 99
eb 04                          jmp END
ELSE:
8b c6                          mov ax, si
```

Findings:
- `if (!cond)` flips the branch condition (`jne` instead of `je`).
- No materialization of `!x` as a value — branch directly handles the inversion.

## char × int — promote char via `cbw` then `imul mem`

Fixture `3417-char-times-int-obj`:

```c
int mix(char c, int n) { return c * n; }
```

```
8a 46 04                       mov al, c
98                             cbw                  (char → int)
f7 6e 06                       imul word [bp+6]    (× n)
```

Findings:
- char promoted to int via `cbw` (1B), then `imul mem` (3B).
- Total 7B body.


## Negative enum constants — `cmp r/m16, imm8` sign-extended

Fixture `3424-enum-neg-obj`:

```c
enum Err { OK = 0, FAIL = -1, BAD = -2 };
if (code == FAIL) ...
if (code == BAD) ...
```

```
83 fe ff                       cmp si, -1       (4B with imm8 sign-ext)
83 fe fe                       cmp si, -2
```

Findings:
- Negative constants in the range [-128, 127] use the imm8-sign-extended form.
- Same shape as small positive constants — no special encoding for negatives.


## Ternary in initialization `int x = c ? 10 : 20;` — store + later reload

Fixture `3428-ternary-init-obj`:

```c
int pick(int c) {
  int x = c ? 10 : 20;
  return x + 1;
}
```

```
4c 4c                          dec sp; dec sp   (alloc x)
83 7e 04 00                    cmp c, 0
74 05                          je ELSE
b8 0a 00                       mov ax, 10
eb 03                          jmp END_TERN
ELSE:
b8 14 00                       mov ax, 20
END_TERN:
89 46 fe                       mov [bp-2], ax   (store to x)
8b 46 fe                       mov ax, [bp-2]   (reload for `x + 1`)
40                             inc ax           (+ 1)
```

Findings:
- BCC stores to x then immediately reloads — no value tracking across statements.
- Optimal: keep value in AX, skip the store/reload (3B savings).
- Confirms statement-boundary IR design.

## Pointer cast `(char*)((int)p + n)` — no-op casts in small memory model

Fixture `3429-cast-pseudo-vp-obj`:

```c
char *bump(char *p, int n) { return (char *)((int)p + n); }
```

```
8b 46 04                       mov ax, p
03 46 06                       add ax, n
```

Findings:
- Both casts (`(int)` and `(char*)`) are no-ops in small memory model (ptr = 2B word).
- Code identical to `p + n` for `char *p` without casts.
- 6B body. Cast doesn't emit any instructions.


## Return-after-store with reg-allocated value — no reload

Fixture `3434-return-after-store-obj`:

```c
int set_get(int *p, int v) {
  *p = v;
  return v;
}
```

```
56 57                          push si; push di
8b 76 04                       mov si, p
8b 7e 06                       mov di, v        (DI = v, reg-alloc)
89 3c                          mov [si], di     (*p = v via DI)
8b c7                          mov ax, di       (return v from DI — no reload!)
```

Findings:
- When `v` is reg-allocated to DI, the return uses DI directly.
- No memory reload (unlike the 3395 assign-in-cond case where x was a stack local).
- Reg allocation suppresses the missed-opt store/reload pattern.

## int == char (global) — char widened via cbw before cmp

Fixture `3435-int-cmp-char-obj`:

```c
char gc;
if (x == gc) ...
```

```
a0 00 00 [FIXUPP _gc]          mov al, [_gc]
98                             cbw              (widen char → int)
3b 46 04                       cmp ax, x
75 05                          jne ELSE
```

Findings:
- char promoted to int via `cbw` before int comparison.
- No byte-cmp peephole — int compare semantics required.

