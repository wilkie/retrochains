# The decompiler IR

How we turn a Borland C++ 2.0 binary back into C. This is the design for the
intermediate representation the decompiler lifts machine code into and emits C
from. It is the third leg of the analysis stack: the **recognizer**
([`../FINGERPRINTS.md`](../FINGERPRINTS.md), `crates/fingerprint/`) decides *which*
compiler produced a binary; this IR decides *what the source was*.

The whole approach rests on one asset the rest of the project built: a
**byte-exact BCC compiler**. That is both the encoder model we read backwards and
the verifier we check ourselves against. So this spec keeps returning to one
question — *does the C we recovered recompile to the original bytes?*

## 1. Goal and the correctness contract

The target is **compiler-accurate C**: C source that, compiled with our `bcc` at
the function's memory model and flags, reproduces the function's bytes exactly.

> A function's recovered C is **correct** iff
> `build_obj(recovered_C)`'s `_TEXT` for that function == the original bytes.

This is stronger than "plausible decompilation" and it is *decidable* — the
compiler is the oracle. It also shapes everything below: the IR doesn't need to
be canonical or optimal, only faithful enough that emitting C and recompiling
closes the loop. Where several C forms recompile to the same bytes, any is
correct; where they don't, the diff tells us which.

## 2. Pipeline and where the IR sits

```
binary ─disassemble─► instructions
       ─recognize───► idioms          (crates/fingerprint/idioms.rs)
       ─lift────────► Lo-IR           § micro-ops, one per idiom
       ─fold/struct─► Hi-IR           § expressions + structured statements (~C AST)
       ─emit────────► candidate C
       ─recompile───► bcc(candidate) vs original  ──repair──┐
                                     ▲                       │
                                     └───────────────────────┘
```

There are **two IR levels**, because two different jobs happen here:

- **Lo-IR** — a linear, typed register-transfer form. The lift from idioms to
  Lo-IR is *mechanical*: each idiom is a known template (it's why we mined the
  catalog), so it maps to a fixed micro-op sequence with no analysis.
- **Hi-IR** — expression trees, lvalues, and structured statements; close to a C
  AST. Getting here is the actual *recovery*: folding the accumulator chains into
  expressions and the branch graph into `if`/`while`.

Keeping them separate means the error-prone analysis (Lo→Hi) is isolated from the
table-driven part (idioms→Lo), and each is testable on its own against the
corpus.

**The emit step is a *seam*, not a fixed mapping.** One byte sequence can spell
out as several equivalent source forms — `*(p+K)`, `p[K]`, and (with recovered
type/provenance) `s->field` or `arr[i]` are all the *same* operation, and where
the compiler supports them they recompile to identical bytes. So the Hi-IR stays
**form-neutral** (an offset deref is `Deref(base + k)`), and a rendering
*policy* — `AccessForm` — chooses the surface syntax. The recompile check is the
**oracle on the choice**: `render_idiomatic` re-renders the recovery under each
form in preference order (subscript first, then pointer arithmetic) and returns
the first whose bytes still match, so `p[K]` is chosen where the compiler builds
it and `*(p+K)` is the automatic, zero-risk fallback. This is the hook a second
pass, a human, or a UI toggle uses to retune the output without ever risking
faithfulness — every rendering is verified before it's offered. (Today the only
policy axis is subscript-vs-arithmetic; struct-field and array forms join it as
type/provenance recovery grows, the same way.) Neither form is universally
compilable — our `bcc` builds a *variable*-index store only as a subscript
(`p[i] = v`) and other shapes only as pointer arithmetic — which is exactly *why*
the verifier, not a fixed rule, decides. Subscript is the unverified default
(`to_c`/`decompile`) because it covers the most recovered cases.

## 3. The value and storage model

BCC's non-optimizing codegen makes the machine state legible. The IR models:

- **The accumulator.** `ax` holds the *current expression value*; `dx:ax` holds a
  32-bit (`long`) value. BCC evaluates every expression into `ax`/`dx:ax` and
  then stores it. This single convention is what makes expression recovery
  tractable — most of the lift is "track what's symbolically in `ax`".
- **Scratch / index registers.** `bx`, `cx` (shift counts, `cl`), and the SI/DI
  register variables (see below).
- **The frame** (`bp`-relative, set up by the `Prologue` idiom):
  - `[bp + N]`, `N ≥ 4` (past the saved bp and return address) → **parameter**.
    Far models push a far return address, shifting the first parameter.
  - `[bp − N]` → **local variable**. The prologue's stack reserve
    (`PrologueLocals`' `sub sp,N`, or `StackReserve2`'s `dec sp; dec sp`) bounds
    the locals' total size.
- **Register variables.** `SaveRegVar`/`RestoreRegVar` (`push/pop si/di`) in the
  prologue/epilogue mark SI/DI as holding `register` locals for the function's
  lifetime. They lift to ordinary named locals; the `register` storage class is a
  hint, not semantics.
- **Globals.** `[disp16]` within DGROUP (near data) or `[seg:off]` (far) →
  **global**, named by its relocation symbol when symbols survive, else a
  generated `g_<addr>`. The relocation table tells near from far.

Mapping a storage location to a C identifier — `[bp−4]` → some local `x` — is the
job of §6 (types) and the symbol pass; the model just names the *slots*.

## 4. Lo-IR: micro-operations

A flat, typed register-transfer list per basic block. The micro-op set is small:

| micro-op | meaning |
|---|---|
| `Load(dst, place)` | `dst ← *place` (place = slot/global/imm/deref) |
| `Store(place, src)` | `*place ← src` |
| `Bin(dst, op, a, b)` | `dst ← a op b` |
| `Un(dst, op, a)` | `dst ← op a` (`neg`, `not`, `inc`, `dec`) |
| `Promote(dst, kind)` | sign/zero-extend (`cbw`: al→ax, `cwd`: ax→dx:ax) |
| `Lea(dst, place)` | `dst ← &place` |
| `Arg(src)` / `Call(dst?, target, argc)` | cdecl push / call (§7) |
| `Branch(cond, label)` / `Jump(label)` | control flow |
| `Enter(frame)` / `Leave` / `Ret(val?)` | prologue / epilogue |

**The lift is the idiom catalog read as a table.** Each recognized
[`Idiom`](../../crates/fingerprint/src/idioms.rs) emits one or a few micro-ops:

| idiom | Lo-IR |
|---|---|
| `Prologue` / `PrologueLocals` / `MscChkstkPrologue` | `Enter(frame)` |
| `EpilogueNear/Far/RestoreSp`, `BccExitJump` | `Leave` / `Ret` (the exit jump targets the epilogue) |
| `LoadLocal` / `LoadGlobal` / `LoadImmAx` / `PointerLoad` | `Load(ax, …)` |
| `LoadLocalByte` / `LoadGlobalByte` | `Load(al, …)` (char) |
| `StoreLocal` / `StoreGlobal` / `StoreImm*` / `PointerStore` | `Store(…, ax/imm)` |
| `AluReg` / `AluLocal` / `AluGlobal` / `AluImm` | `Bin(ax, op, ax, …)` |
| `Grp3` (imul/idiv) | `Bin(dx:ax, mul/div, …)` |
| `Shift1` | `Bin(ax, shl/shr/sar, ax, 1)` |
| `Cbw` / `Cwd` | `Promote(…)` |
| `LeaLocal` | `Lea(ax, local)` |
| `IncDecReg` / unary `Grp3` | `Un(…)` |
| `PushAx` / `NearCall` / `FarCall` / `CdeclPop*` | `Arg` … `Call` (§7) |
| `Jcc` / `ShortJump` | `Branch` / `Jump` |
| `SaveRegVar` / `RestoreRegVar` | (frame metadata; no op) |

Gaps (unrecognized bytes — the long tail) become an opaque `Asm(bytes)` micro-op:
the lift degrades gracefully, the function is still structured around what we *do*
recognize, and the recompile check simply fails on functions still containing
`Asm` (a precise "not yet decompilable" signal rather than a wrong answer).

This lift is built:
[`crates/decompile/src/lo_ir.rs`](../../crates/decompile/src/lo_ir.rs).
`lift(code)` runs `fingerprint::recognize`, decodes each idiom's masked-out
operands (displacements, immediates, register fields) into the micro-ops above,
and coalesces unrecognized runs into a single `Asm`. Every `LoInsn` carries the
byte `Span` it lifted from — the provenance §8 maps mismatches back through.
Operand decode is the only real work, and it's mechanical: the idiom *is* the
instruction shape, so reading e.g. a `[bp±disp]` displacement is a fixed byte
offset, not analysis.

## 5. Hi-IR: expressions and structured statements

Close to a C AST, plus provenance (§8):

```
Stmt   = Assign(LValue, Expr) | Compound(LValue, op, Expr) | Call(target, [Expr], result?)
       | If(Expr, [Stmt], [Stmt]) | While(Expr, [Stmt]) | For(…) | Do(…)
       | Return(Expr?) | Break | Continue | Switch(Expr, cases)
Expr   = Const | Var | Global | Param | Binary(op, Expr, Expr)
       | Unary(op, Expr)   // `-e` (neg), `~e` (not); logical `!e` is its own node
       | Deref(Expr) | AddrOf(LValue) | Index(Expr, Expr) | Field(Expr, name)
       | Cast(Type, Expr)  // a narrowing `(char)x` — see §6
       | CallExpr(target, [Expr])
LValue = Var | Global | Deref | Index | Field
```

Two recoveries produce it:

- **Expression folding** — symbolically execute the accumulator across a basic
  block. `Load(ax, b)`, `Bin(ax, +, ax, c)`, `Store(a, ax)` collapses to
  `a = b + c`. Each `Store` (or `Arg`, or branch on `ax`) *flushes* the current
  `ax` expression; a fresh `Load(ax, …)` starts a new one. Because BCC doesn't
  keep values live across statements in registers (it stores back to slots), the
  accumulator state resets at statement boundaries — bounding the analysis.
- **Control-flow structuring** — build a CFG from `Branch`/`Jump`, then recover
  `if`/`while`/`for`. BCC's loop shape is stereotyped (see the for-loop notes:
  the test sits *between* the step and the body, with a jump-back folded into the
  initial `if`'s else), so pattern-matching the templates is more reliable than
  generic interval analysis — and, again, the recompile check adjudicates. A
  **ternary** `cond ? t : f` is the same diamond as an `if`/`else`, but both arms
  reduce to a single accumulator *value* with no statements (`<t→ax>; jmp end;
  else: <f→ax>`); detected before the `if` is built, it folds to an
  `Expr::Ternary` whose merged value is *seeded* into the following straight-line
  run (a `pending_acc` the next fold picks up), so the consumer — a `return`, a
  store — sees it. This is the one place an expression spans a branch, so it must
  be recognized at the structurer, not the linear fold.

**`Compound` vs `Assign` — an in-place distinction that is not cosmetic.** BCC
codes `x op= y` differently from `x = x op y`: a register variable, global, or
memory operand that is *also* the destination is updated in a single instruction
(`inc si`, `add si,5`, `inc word [g]`, `di += si`), whereas the general
assignment routes through the accumulator (`mov ax,si; inc ax; mov si,ax`). The
two recompile to different bytes, so they must recover to different source. The
fold emits `Stmt::Compound(lv, op, rhs)` for the single-instruction in-place form
(`Un{inc/dec}` on a variable, or `Bin{dst==lhs, +−&|^}`) and keeps the
load-op-store sequence as a plain `Assign`. Emission spells `Compound` as
`lv++`/`lv--` for a ±1 step (BCC codes `x += 1` and `x++` identically — both
`inc`) and `lv op= rhs` otherwise; the `for`-loop step accepts either form. The
`ff 06/0e disp16` global inc/dec is its own recognizer idiom (`Grp5Global`).
Still load-op-store (so still a plain `Assign`): `char` in-place (`++c` computes
in `al`), which keeps the byte form.

Both are built (`hi_ir.rs`). The structurer recurses over the Lo-IR by index,
matching BCC's stereotyped shapes directly: a **forward** `cmp` + conditional
branch that *skips* the then-block is an `if` (so the source condition is the
branch's, **negated**); an unconditional jump at the then-block's tail past a
second block makes it `if/else`; a **loop-rotated** header jump to a bottom test
that conditionally branches *back* to the body is a `while` (the branch is the
loop-continue condition, taken verbatim). Nesting falls out of the recursion —
`if`-in-`while`, sequential `if`s, accumulation loops all round-trip. Conditions
recover from the `cmp` operands plus a `Jcc`-nibble → relational-operator table
(signed `< <= > >= == !=`; unsigned and flag-only codes mark incomplete). A `cmp`
operand may be the accumulator (`mov ax,i; cmp ax,n` when comparing two memory
operands — x86 can't compare memory-to-memory), resolved from the load before
the `cmp`; this is what lets a loop or `if` bound be a parameter or global.

**`for` loops are recovered** as `for` syntax. BCC lowers `for (init; cond;
step) { body }` to `init; goto test; loop: body; step; test: if (cond) goto
loop` — exactly the loop-rotated `while` shape with the step appended to the
body. So the structurer first recovers it as a `while`, then a post-pass
re-renders it: when the loop variable (one named in the condition) is assigned
just before the loop and stepped at the body's tail, it folds back to
`for (init; cond; step) { body-without-step }`. A loop whose body is *only* the
step stays a `while` (an empty-body `for` lowers differently — the recompile
check catches that, so the pass requires a real body). For and the equivalent
while lower identically, so this is a faithful re-rendering the oracle confirms.

**`do`/`while` loops are recovered** as `do { } while (cond);`. BCC lowers a
do-while to `loop: body; cmp; if (cond) goto loop` — a **backward** conditional
branch with *no* header jump (the body always runs once before the test). That
absence is the discriminator: a loop-rotated `while` jumps to its bottom test
first, a `do` does not. So when the structurer meets a backward branch whose
target sits at or before the `cmp`, it folds the body (`fold_linear` over the
body region, returning the test accumulator), reads the condition off the
`cmp`/`Jcc` taken verbatim (not negated — the branch *continues* the loop), and
emits `Stmt::Do`.

**`switch` (compare-chain form) is recovered.** A small `switch` (≤ 3 cases)
BCC lowers to a compare-chain: `cmp ax,K1; je T1; cmp ax,K2; je T2; …; jmp
default`, then the case bodies laid out contiguously. The structurer detects a
run of ≥ 2 consecutive `cmp ax,Ki; je Ti` links (the scrutinee in `ax`,
recovered from the run's accumulator) ending in an unconditional jump; each case
body runs from its target to the next case's (or the no-match block); the
no-match block is the **post-switch code** (so an absent `default` is the code
that follows, matching C's fall-through). It emits `Stmt::Switch(scrutinee,
[(value, body)…])`. A case body ending in a jump to that post-switch block is a
**`break`** (`Stmt::Break`); one ending in a jump to the epilogue is a `return`.
So `case 1: r = 10; break;` and `case 2: return 99;` both recover, in the same
switch. A *dense* switch (≥ 4 contiguous cases) BCC lowers to a
**jump table**, which is also recovered: `mov bx,scrut; {dec|sub bx,base}; cmp
bx,N; ja default; shl bx,1; jmp cs:[bx+table]`, then the case bodies, then the
`(N+1)`-entry offset table appended after the epilogue. The structurer reads the
table from the raw `_TEXT` bytes — each entry is a case body's offset — and maps
case value `base+k` to the body at `table[k]`. (The indirect jump `2e ff a7
disp16` is a `JumpTableJmp` recognizer idiom — `IndirectJump { disp }` — so the
linear lift consumes the `disp16` cleanly instead of mis-decoding the table
boundary; the structural range is trimmed to the last `ret` so the trailing
table data isn't structured as code.) A table entry equal to the no-match block is a **gap** (that
index has no case — `case 5` after a missing `4`); consecutive equal entries are
**fall-through** (case values sharing a body, rendered as empty lead cases:
`case 1: case 2: body`). The bodies are laid out in value order, so the present
entries must be non-decreasing — an out-of-value-order source switch lays them
out non-monotonically and declines (sound, not mis-shaped). A sparse switch
(`case 1,2,4`) BCC emits as a compare-chain, not a table, so it's recovered
there; a `default:` block recovers as the post-switch code when it
returns, and as a real `default` arm (the third `Stmt::Switch` field) when it
ends in `break` — the no-match block then jumps to a *further* continuation, so
the cases break there too, not to the no-match target. A **`char`/`unsigned
char` scrutinee** dispatches the same way but loads and widens the index first:
`mov al,[c]; {cbw | mov ah,0}; {dec|sub ax,base}; mov bx,ax; cmp bx,N; …` — the
byte is loaded into `al`, widened (`cbw` ⇒ signed, `mov ah,0` ⇒ `unsigned`),
normalized in `ax`, then copied to `bx`. The recovered switch marks the
scrutinee `char` (and `unsigned` for the zero-extend) so the regenerated prologue
re-emits the byte load and the same widen, recompiling byte-exact. (A `long`
scrutinee uses a runtime search loop, not a table, so it's out of scope and
declines.)

**Early returns / multi-exit are recovered.** Every `return <expr>` is `mov
ax,val; jmp epilogue` — a jump to the shared epilogue (which begins at the
register-variable restores, if any, then `Leave`/`Ret`). So the fold treats a
jump-to-epilogue as a `Return` of the accumulator, and the structurer no longer
mistakes a then-block's return-jump for an `if/else` skip (a skip targets a
*continuation*, a return targets the *epilogue*). `if (a>0) { return a; } return
0;`, sequential guards, an `if/else` where both arms return, and a `return`
inside a loop all recover. (The earlier "bail on an exit jump with no `ret` in
the run" guard is gone — that *was* the early return.) A side effect: the `or
r,r` register-variable test combines with a *signed* `Jcc`, not just `jz`/`jnz`,
so it recovers the full `r <rel> 0` family (`if (a>0)` → `or si,si; jle`), with
equality still rendered as the bare/negated variable so it recompiles to `or`.

**Register variables are recovered** (§3): a `Var` is either a stack slot or a
`si`/`di` register variable, and the reg-var data-flow forms lift uniformly —
`mov ax,si` / `mov si,ax` route the accumulator through the variable, `mov
si,imm` is a direct assignment, `xor si,si` is the zero idiom, `or si,si; jcc`
is the truthiness test (`x != 0`). Both kinds emit as plain `int` locals;
recompiling a plain `int` reproduces BCC's deterministic register allocation, so
the emitter just declares variables in allocation order (reg vars `si` then `di`,
then stack slots closest-to-bp first) and BCC re-derives the same storage. This
closes the loop on **default** BCC output — multi-variable functions using both
`si` and `di` round-trip byte-exact.

Expression folding is built:
[`crates/decompile/src/hi_ir.rs`](../../crates/decompile/src/hi_ir.rs) +
[`emit.rs`](../../crates/decompile/src/emit.rs). `recover(code)` folds the
single-block accumulator into `Assign`/`Return` statements; `decompile(code)`
emits C — but only for a *fully* recovered function. Anything not yet modelled
(control flow, calls, params, globals, byte/long widths, pointers) sets
`Function::complete = false` and `decompile` returns `None`, so a half-recovery
is never presented as done. With this the §8 loop **closes end-to-end** for the
first time: a battery of tests compiles a C snippet, decompiles its `_TEXT`
purely from the bytes, and `verify`s that the recovered C recompiles to the
identical bytes — straight-line `int` returns, local assignments, and
arithmetic/bitwise chains (including the `x - 2` ⇒ `add ax,-2` sign asymmetry).
Control-flow structuring is the next increment. (Closing the arithmetic case
also added the `alu ax, imm16` accumulator idioms — `05/2d/…` — to the
recognizer; until then `add ax,3` was an `Asm` gap.)

The single-accumulator model has one extension for binary ops whose operands
*both* need register evaluation (two `char` operands — each needs a `cbw`, so
neither can be a memory operand — or two calls). BCC spills the left operand
(`push ax`), evaluates the right into `dx` (`mov dx,ax`), restores the left
(`pop ax`), and combines (`<op> ax,dx`). The fold reuses the call-argument push
stack for the spill: `pop ax` restores the spilled left, a `mov dx,ax` parks the
right in a `dx_temp`, and `<op> ax,dx` folds to `Binary(op, left, right)` —
operand order preserved, so `x - y` doesn't become `y - x`. This is what
unblocked `char` arithmetic (`x + y`, `a[0] + a[1]`, `char` params).

## 6. Types and type recovery

The lattice: `char` (8), `int` (16, the default), `long` (32, `dx:ax`),
near/far pointer (by model), `unsigned` variants, array, struct, `void`.

Recovery is driven by the idioms, not guessed:

- **Width** — byte ops (`LoadLocalByte`, `c6`) ⇒ `char`; word ops ⇒ `int`;
  `dx:ax` pairing ⇒ `long`. **But a byte op isn't always a `char`:** a
  `mov al,[x]` that reads the *low byte of an `int`* is a narrowing `(char)x`,
  not a `char` load. A pre-pass records the **word-accessed** slots/globals (a
  full-register or word-immediate load/store — byte accesses use `Byte(_)` /
  `StoreImmByte`); a byte load of one of those is wrapped in `Cast(Char, …)`
  rather than char-marking the variable. This keeps `int x` typed `int` (so a
  mixed `int`/`char` frame isn't mis-modelled as a `char` array — which fed `bcc`
  an `a[i]=a[j]` it crashed on), and it reproduces the byte load: a plain `c = x`
  re-evaluates `x` at word width (`mov ax,[x]`), whereas the recovered
  `c = (char)x` reads the low byte (`mov al,[x]`). The cast is **dropped** inside
  an in-place `char` compound (`c |= n`, not `c |= (char)n`) — the byte op
  already narrows — and likewise the `char op= int` rhs stays `int`.
  *(Long is built for constants and pass-through: a
  `dx` tracker pairs the high word with the low — `xor dx,dx` / `mov dx,imm`
  (constant high) or `mov dx,[lo+2]` (a variable's high slot) — and the
  following `mov ax,…` forms the `long`: `(high<<16)|low` for a constant, or the
  variable at `lo` (which is recorded in `long_vars` and declared `long`). The
  return type is `long` when the returned accumulator is. A `long` occupies two
  slots `(lo, lo+2)`. A **`long`-local constant assignment** is a store *pair*
  (high word first, then low: `mov [hi],imm_hi; mov [lo],imm_lo`), folded into a
  single `long` assignment. The catch is that two adjacent `int` stores are
  *byte-identical* at the store site, so the disambiguation comes from the
  read-back: a pre-pass collects the slots read as a `dx:ax` pair (`mov
  dx,[lo+2]; mov ax,[lo]`) — the genuine `long` locals — and only those slots
  fold their store pair (otherwise the high slot would double as a separate
  `int` variable and the frame would be double-counted, which a guard in
  `recover` still rejects for the cases not yet folded). `long`
  arithmetic is built: `a + b` is `add ax,[b.lo]; adc dx,[b.hi]` and `a - b` is
  `sub`/`sbb`, so a `dx` tracker pairs the low-word add with the `adc` that
  completes it (the operand is a `long` variable `[lo]`/`[lo+2]` or a constant
  `(hi<<16)|lo`). Two subtleties: (1) the **long-parameter layout** — a `long`
  param occupies two slots, so the signature walks param offsets by width (4 for
  `long`, 2 otherwise), filling unread gaps with `int`; this also fixed the
  earlier offset-coincidence hack. (2) a **negative `long` constant** is emitted
  as a *subtraction* (`x - K`, an `Expr::LongConst` with an `L` suffix) — BCC
  mis-folds `x + (negative long literal)` (the literal reads as `unsigned int`
  and loses its high word), but compiles `x - <positive>` correctly. The `adc`
  (`13`) / `sbb` (`1b`) memory/register forms were the needed recognizer
  additions.)* *(Built: a `char` var is one accessed at
  byte width — `8a`/`88`/`a0`/`a2`, `StoreImmByte`, the `80` byte group-1 compare
  — recorded in `Function::char_vars` and declared `char` instead of `int`; `cbw`
  is the implicit `char`→`int` promotion, folded as a no-op since emitting the
  `char` source recompiles to it. Width must be right: it changes both the access
  encoding and the storage size/offsets. A `char` local BCC promotes to a byte
  register variable — `dl` etc., the `char` analogue of `si`/`di` — is recovered
  too: `Var::ByteReg`, always `char`, with the byte data-flow (`mov dl,imm` /
  `mov al,dl`), the byte compare, and the `or dl,dl` truthiness test. That test
  exposed a subtlety: `if (x)` (an `or r,r`) and `if (x != 0)` (a `cmp`) recover
  to the same shape but emit differently, so a register-variable truthiness test
  recovers as the **bare** variable (`if (x)` / `if (!x)` via `Expr::Not`), not
  `x != 0` — `!= 0` would recompile to a `cmp`, not the original `or`. Recognizer
  additions across the `char` work: `80 /r … imm8` byte group-1, `c6 06` global
  byte-store, and the byte register idioms `b0+r`, `8a/88` mod=11, `02/0a/…`
  mod=11. The **`char` return type** is built too: a `char`-returning function
  leaves its value in `al` (a byte) with no widening, so the discriminator is
  local — a byte-register write (`mov al,…`) *immediately before* the
  return-jump means `char`, where an `int` return would have a `cbw` as that
  last instruction. (Returning an `int` value from a `char` function is *not*
  distinguishable — `mov ax,[a]` is identical to an `int` return and the low
  byte is the result — so it recovers as `int` and recompiles byte-exact
  anyway.) The byte-load fold also learned the `mov al,imm8` immediate form, so
  `char f() { return 5; }` recovers.)*
- **Signedness** — `Cbw`/`Cwd`/`sar`/`idiv` ⇒ signed; zero-extend / `shr`
  / `div` ⇒ unsigned. *(Multiply/divide built: `imul`/`idiv` produce `dx:ax`,
  but an `int` result is the low word, so `a * b` (`imul [b]` or `mov dx,K;
  imul dx`) folds to `a * b` and `a / b` (`cwd; idiv [b]`) to `a / b` — `cwd` is
  the dividend setup, a no-op for the fold. `a % b` is the same `idiv` followed
  by `mov ax,dx` (the remainder): the fold remembers the `(dividend, divisor)` at
  the `idiv` and synthesizes a `Mod` operator when the `dx` result is read. Only
  signed `imul`/`idiv` for now; the unsigned `mul`/`div` are deferred. Division
  by a constant lowers to `mov bx,K; cwd; idiv bx` rather than a memory `idiv`,
  so the fold resolves an `idiv bx` divisor through the `bx` const tracker — `a /
  2` (signed), `a / 2` (unsigned `div`), and `a % 2` all fold and round-trip. (A
  `long` constant divide is a runtime helper call, not `idiv`, so it stays
  deferred.) Needed recognizer additions: `f7` with a
  memory operand (`imul/idiv [bp±N]`/`[disp16]`), which was an `Asm` gap that
  even mis-lifted `idiv [bp+N]` as a stray `jle`. Unsigned built too: an unsigned
  compare (`jb`/`ja`/`jbe`/`jae` → `ULt`/`UGt`/…) marks its operands `unsigned`,
  and a logical `shr` marks the shifted value `unsigned` — those declare
  `unsigned` so the compare/shift re-emits unsigned (`jbe` not `jg`, `shr` not
  `sar`); the unsigned relations print the same C token, the declared type drives
  the re-emission. BCC unrolls a constant shift into shift-by-1s, so the fold
  collapses `(x >> a) >> b` back to `x >> (a+b)` — re-emitting nested shifts would
  make the intermediate signed (an outer `sar`). `unsigned char` is built: the
  zero-extend `mov ah,0` (vs `cbw`) marks the accumulator's `char` `unsigned`. A
  byte comparison (`cmp byte ptr [c],5`) lifts to a distinct `CmpByte` op that
  marks its operands `char` — without it a `char` *only* ever compared (a char
  parameter, say) would declare as `int` and re-emit a word `cmp`; this fixed a
  latent mismatch for signed `char` comparisons too.)*
- **Pointers** — `Lea` of a slot, or `PointerLoad`/`PointerStore` through
  `[si]/[di]`, ⇒ a pointer; near vs far from the relocation form. *(Built for
  near `int *` reads: `*p` is `mov bx,p; mov ax,[bx]`, so a `bx` tracker holds
  the pointer value and `mov ax,[bx]` lifts to `Expr::Deref(p)`; `&x` is `lea
  ax,[bp+disp]` → `Expr::AddrOf(x)`. A dereferenced variable is recorded in
  `ptr_vars` and declared `int *`. The `[bx]` deref needed a recognizer
  addition — `8b/89/8a/88` with mod=00 rm=111, distinct from the `si`/`di` `PTR`
  mask — and a fix to the 16-bit memory-`rm` decode (`100`→`si`, `101`→`di`,
  `111`→`bx`, not the register encoding). Pointer **writes** (`*p = v`,
  `*p = const`) recover as `LValue::Deref` (`mov [bx],ax` / `mov [bx],imm`), and
  a two-deref expression (`*p + *q`) recovers via `add ax,[bx]` with the second
  pointer reloaded into bx — both needed recognizer additions (`c7 07` store-imm
  and `03/2b/… 07` ALU through `[bx]`) and were *also* gated by two `bcc` panics
  on stack-resident pointers, since closed fixture-driven (fixtures 4271/4272).
  A dereference inside a condition is recovered too: `fold_linear` returns the
  value it leaves in the accumulator, so the condition reads `*p` (or any
  computed test value) from the test region's fold — `if (*p > 0)` is
  `mov ax,[bx]; or ax,ax; jle`, where `or ax,ax` is the accumulator truthiness
  test and `ax` holds `*p`. This subsumed the old one-instruction-back operand
  resolver: a `cmp`/`or` register operand now resolves to whatever the test run
  computed, however many instructions it took. **`char *` is built**: the
  discriminator is the deref *width* — `mov al,[bx]` (a byte load) vs `mov
  ax,[bx]` (a word load) marks the pointer a `char *` rather than an `int *`
  (recorded in `char_ptr_vars`, declared `char *`, disjoint from `ptr_vars`).
  The deref'd value is a `char`, so an `int` context promotes it with the usual
  no-op `cbw`. The write side mirrors it: `mov [bx],al` (`*p = v` storing a
  `char`) and `mov byte ptr [bx],imm8` (`*p = const`) — the latter the one
  recognizer addition (`c6 07 ib`, `StoreImmByteDeref`, the byte analogue of the
  `c7 07` word store-imm). The `char` *return* type these often want is built
  separately (see the Width note above), so `char f(char *p) { return *p; }`
  now round-trips too. **Constant-offset derefs** (`p[K]` / `*(p+K)`) are built:
  `mov ax,[bx+disp8]` (and the byte `mov al,[bx+disp8]`) — a new
  `PointerLoadDisp8` recognizer idiom (`8b/8a` with mod=01 rm=bx) lifted to a
  `Place::DerefDisp(reg, disp)`. The fold divides the byte displacement by the
  pointee stride (2 for `int`, 1 for `char`) to recover the element index and
  emits `*(p + K)` (a `Deref` of an `Add`), which BCC recompiles to the same
  `[bx+K*stride]`; `K == 0` collapses to a plain `*p`. An odd displacement on an
  `int *` isn't a clean index, so it bails. This recovery is what the
  fixture-driven `bcc` work above enabled: the stack-resident `p[K]` read/write
  and `*(p+K)` read paths all panicked until fixtures 4273–4276 closed them, so
  the recovered C now recompiles instead of trapping. The **write** side
  mirrors it: `mov [bx+disp],ax` (`PointerStoreDisp8`) and `mov word ptr
  [bx+disp],imm16` (`StoreImmDispDeref`) recover as `*(p + K) = value` (an
  `LValue::Deref` of the same offset-pointer expression), for a constant or a
  variable RHS. The surface form (`p[K]` vs `*(p+K)`) is the rendering seam's
  choice, not baked in — see §2. **Variable index** (`p[i]`) is built too:
  `mov ax,i; shl ax,s; mov bx,p; add bx,ax; mov ax,[bx]` — the index is scaled
  to a byte offset (`i << s`, `s = log2(stride)`), so the fold strips the shift
  to recover the C-level index and the `add bx,ax` makes `bx = Deref(p + i)`.
  The **provenance** of the base register is the array-vs-pointer
  discriminator: a base loaded from memory (`mov bx,[p]`) is a *pointer* index,
  so this recovers `p[i]`; a base from `lea bx,[bp-N]` would be a local *array*
  (`a[i]`) — that sibling, with the index arriving in `bx` and the base in `ax`,
  stays unhandled pending array-extent recovery. Scoped to `int` writes — a
  `char` indexed-immediate write hits a separate `bcc`/TASM gap (`byte ptr
  [bx+disp],imm`).)*
- **Promotions** — `Cbw`/`Cwd` become explicit `Cast` nodes, so the emitted C's
  implicit promotions recompile to the same `cbw`/`cwd`.
- **Aggregates** — a `Lea base` then indexed access ⇒ array; a constant field
  offset added before a deref ⇒ struct field. Layout is checked the only way that
  matters: recompiling and diffing. *(Local `int` arrays are built. A constant
  array index folds to a direct `[bp+disp]` slot, so `int a[M]` surfaces as
  scalar slots — and only the *accessed* ones, which under-allocates the frame
  (the old recovery silently MISMATCHED: its scalars produced the wrong frame).
  BCC's frame layout is the key: locals are allocated in declaration order
  top-down from `bp`, and an array is one block with element 0 at its lowest
  address. So a post-pass reads the `Enter` frame `N` and asks whether the
  recovered `int` slots *are* the whole top-packed scalar layout — offsets
  exactly `-2,-4,…,-2k` filling `N`. If so they're genuine scalars and stay so;
  otherwise the frame is modelled as one `int a[N/2]` and each slot at `off`
  becomes `a[(off+N)/2]`, which reproduces the identical `[bp+disp]` access, so
  the array always round-trips. The array-vs-scalar call is principled but
  inherently partial: a *fully*-accessed `a[M]` is byte-identical to `M` scalars
  and recovers as scalars — only the unused space of a sparse array reveals it.
  The unambiguous signal is a **variable index**: `a[i]` is a `lea` of the array
  base plus a scaled index (`lea ax,[bp-N]; <index in bx>; shl bx,s; add bx,ax;
  mov ax,[bx]`). It mirrors the pointer `p[i]` exactly but for the base's
  **provenance** — a `lea` (the address of a local) vs a loaded pointer — and the
  register roles flip (the array scales its index in bx, the pointer in ax). So
  the `add bx,ax` fold has two branches: a base that's an `AddrOf` (the `lea`) is
  an array → `a[i]`; a base that's a loaded `Var` is a pointer → `p[i]`. The
  `lea` proves the array even with *no* constant access (`int a[8]; return a[i]`
  recovers from the `lea` and frame alone), and it's the lever for the
  non-sole-array case. The emit spells `Deref(&a[0] + i)` as the array name
  `a[i]` (or `*(a + i)`), not the literal `(&a[0])[i]`. **Mixed frames are
  partitioned on the `lea` anchors**: each lea base starts an array running up to
  the next boundary (the next-higher accessed slot or lea base), the rest stays
  scalar — so `int x; int a[4]` recovers as a scalar plus a 4-array, not one
  merged `a[5]` (and round-trips, which the merge sometimes didn't). A
  single-element span is an address-taken scalar (`&x`), not an array. The emit
  then orders locals **closest-to-`bp` first**, because BCC lays them out in
  declaration order top-down — only that order reproduces the offsets. A frame
  with *no* lea anchor stays the sole-array fallback (genuinely ambiguous: a
  constant-index-only `int x; int a[4]` is byte-identical to one `a[5]`).
  **`char` arrays** join `int`: the element type is carried on the `ArraySpec`
  (stride 1 vs 2), so a frame of byte-accessed slots reconstructs a `char a[M]`.
  Its layout is subtler — stride 1, any offset parity (`char a[4]`@`-4..-1`), and
  a `char` frame is padded up to even (a lone `char` is frame 2 at `-1`), which
  the scalar check accounts for. A variable-only-indexed `char` array (no
  constant element access, so no `char`-typed slot) is typed from the **byte
  deref alone** — `mov al,[bx]` through `&a[0] + i` records the base as a `char`
  array, the only element-type evidence it has. **`long` arrays** complete the
  axis (`ArraySpec` carries an `ArrayElem` of char/int/long → stride 1/2/4): a
  `long` element is a `dx:ax` word pair, so a variable index scales by `<<2`
  (the whole shift chain is stripped to recover the index) and reads the pair
  through the element address (`mov dx,[bx+2]; mov ax,[bx]`). That pair deref —
  like the `char` byte deref — types the array `long` with no slot evidence, so
  `long a[8]; return a[i]` recovers. (The constant-index `long` array, whose
  store is a word *pair* that isn't yet folded into an array element, stays
  unmodelled — when such a variable-indexed element is recognized but the frame
  pass can't reconstruct the array, the recovery bails rather than emit a
  dangling `&v[i]`.) The element type also reads off the **index scale**: a `<<2`
  is a 4-byte stride, the only `long` signal a *store* gives — a `long`-array
  element store writes just the low word (a BCC codegen quirk), so it's not a
  clean `long` assignment and the recovery *declines* it (sound, not mis-shaped
  as an `int` array the stride-4 index would betray). Pointer slots still opt
  out; a frame mixing element types opts out too.)*

Near globals are built (`hi_ir.rs`). A global is a `Var::Global(offset)` — and
crucially the offset is *not* a placeholder like a call target: it's the real
DGROUP-relative displacement (`int a; int b;` ⇒ `a`@0, `b`@2), kept verbatim in
`_TEXT`. So globals are recovered like stack slots — distinguished by offset and
reproduced by declaring file-scope `int gv1, gv2, …` in offset order, sized by
the highest offset used, so recompiling re-derives the same displacements. Only
even (word/`int`) offsets are taken for now; odd offsets (`char` globals,
struct/array interiors) mark the function incomplete. Closing the
global-in-a-condition case also added the `cmp [disp16], imm8` group-1 form to
the recognizer (until then `cmp word ptr [global], 0` was an `Asm` gap).

## 7. Calling-convention recovery (cdecl)

BCC is cdecl: arguments pushed **right-to-left**, a near/far `call`, and
**caller** cleanup. The pattern is a window the lift recognizes directly:

```
Arg(eₙ) … Arg(e₁)  Call(target)  Cdecl­Pop(k)   ⇒   r = target(e₁, …, eₙ)
```

The cleanup size `k` (`CdeclPop1`'s `pop cx` = 2 bytes, `CdeclPopN`'s `add sp,k`)
gives the argument byte count; combined with each pushed expression's recovered
type it yields the argument list. A result is whatever the following code reads
out of `ax`/`dx:ax`. (MSC differs only in cleanup style — `add sp,k` always — and
the chkstk prologue; the same window works, which is why the recognizer is
multi-toolchain.)

Parameters and calls are built (`hi_ir.rs`). Parameters are `Var::Param`
(`[bp+disp]`, `disp ≥ 4`) and emit into the signature (`int p1, int p2, …`),
sized by the highest slot used. The fold assembles `Arg`s into a pending list,
reverses it at the `Call` (push order is RTL), and leaves the result in the
accumulator — so `x = g(3)` and `g(a, b)` recover directly, and a discarded
result (`g(3);`) surfaces as an `ExprStmt` (flushed when the next op would
overwrite the accumulator). **Two facts make the callee's identity irrelevant to
the bytes:** a `call` is a placeholder `e8 00 00` patched by a relocation, so one
declared K&R extern (`extern int g0();`) recompiles any call site; and an
explicit `return <expr>` carries BCC's redundant exit jump (`eb 00`) while a void
fall-off doesn't — that jump is the signal distinguishing "return the
accumulator" from "the trailing value was a discarded call". A return *inside* a
branch (a multi-exit shape) isn't structured yet, so it's detected (an exit jump
in a sub-block with no `Ret`) and marked incomplete rather than mis-recovered.

**Multi-function programs.** A `_TEXT` segment usually holds several functions
laid out one after another (a helper, then `main`, …). `recover_program` splits
it on the prologue: a function starts at an `Enter` that *opens* a new function
— the first one, and any that follows the previous function's `ret` (so the
`dec sp; dec sp` 2-byte-local reservation, which also lifts to `Enter`, isn't
mistaken for a boundary). Each window is recovered independently, with absolute
offsets preserved (the lift keeps byte offsets, so internal branches and embedded
jump tables still resolve against the full segment). Functions emit as `f0, f1, …`
in `_TEXT` (definition) order — the order BCC lays them out, so reproducing it
reproduces each intra-module call's forward/backward resolution.

This is where the callee's identity stops being irrelevant: a `call` to a
*local* function is **not** a `e8 00 00` placeholder — BCC resolves the
displacement at compile time, so the near call carries a real rel16 to the
callee's prologue. The lift computes that target (`Call { target }`), and program
emit names it (`f1` calling `f0`) instead of an opaque extern — only an
*external* call (target matching no function start: the `0000` placeholder points
just past itself) stays `g0`. A lone function takes the single-function path
unchanged. Programs that touch file-scope globals decline for now (one shared
data-segment layout across functions isn't reconciled yet) — sound, not wrong.

## 8. Provenance and the verify/repair loop

Every Lo-IR and Hi-IR node carries the **byte range it lifted from**. This is what
turns the recompile check from pass/fail into a repair signal:

1. Emit C from Hi-IR.
2. Recompile with `bcc` (correct model/flags) → bytes.
3. Diff against the original function. On a match, the function is **done**.
4. On a mismatch, the diverging byte offset maps back through provenance to the
   Hi-IR node that produced it — the candidate to re-examine (wrong operator,
   missed promotion, mis-structured branch).

The recompile-verify harness is the engine for this loop; it's decoupled from
the IR — it only needs `(candidate C, target bytes)`. It's built:
[`crates/decompile/src/verify.rs`](../../crates/decompile/src/verify.rs).
`verify(candidate_c, opts, target)` runs steps 1–3 (compile the candidate the
same `-c` path `bcc` drives, pull the first CODE segment back out, diff) and
returns either `Outcome::Match` or an `Outcome::Mismatch` carrying both byte
runs and `first_diff` — the offset step 4 maps back through provenance. A
compile failure is a distinct `HarnessError::Compile` (malformed C) rather than
a mismatch (well-formed C, wrong bytes), because the repair loop reacts to the
two differently. The IR's job is to be *localizable* so the harness's diffs are
actionable.

## 9. Scope, non-goals, open questions

**In scope now:** BCC 2.0's stereotyped, non-optimizing codegen for C — the
constructs the plateau'd idiom catalog covers (~92% of fixture code bytes):
integer/char/long scalars, the ALU/compare operators, locals/params/globals,
near pointers and arrays, cdecl calls, `if`/`while`/`for`.

**Non-goals (deliberately deferred):**
- The long-tail idioms (segment ops, string instructions, `int`, FP/`fwait`).
- Optimized code — BCC 2.0 barely optimizes, so we don't model it yet.
- C++ proper (classes, name mangling) — the corpus is all C.
- Whole-program structure (calls *between* functions, global layout) — the unit
  here is one function.

**Open questions:**
- **Names without debug info.** Locals/params recover as slots; mapping `[bp−4]`
  to a meaningful name needs either the (optional) BCC debug COMENTs or generated
  names. Recompilation doesn't depend on names, so this is cosmetic — but it's
  what makes output *readable*.
- **Register-allocation inversion.** Which source variable lives in SI vs DI
  across a function (BCC's allocator is deterministic — see the reg-pool specs —
  so this is recoverable, not guessed).
- **Switch tables**, struct field/layout recovery, and far/huge addressing
  modes — each is a recompile-checkable extension of §5–6.

The idiom catalog is half this spec already; the work is the Lo→Hi recovery
(§5) and wiring the verify loop (§8). Both are gated by the same oracle the rest
of the project runs on.
