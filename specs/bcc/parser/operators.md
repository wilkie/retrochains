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

