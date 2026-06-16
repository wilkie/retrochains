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

## 5. Hi-IR: expressions and structured statements

Close to a C AST, plus provenance (§8):

```
Stmt   = Assign(LValue, Expr) | Call(target, [Expr], result?)
       | If(Expr, [Stmt], [Stmt]) | While(Expr, [Stmt]) | For(…) | Do(…)
       | Return(Expr?) | Break | Continue | Switch(Expr, cases)
Expr   = Const | Var | Global | Param | Binary(op, Expr, Expr) | Unary(op, Expr)
       | Deref(Expr) | AddrOf(LValue) | Index(Expr, Expr) | Field(Expr, name)
       | Cast(Type, Expr) | CallExpr(target, [Expr])
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
  generic interval analysis — and, again, the recompile check adjudicates.

## 6. Types and type recovery

The lattice: `char` (8), `int` (16, the default), `long` (32, `dx:ax`),
near/far pointer (by model), `unsigned` variants, array, struct, `void`.

Recovery is driven by the idioms, not guessed:

- **Width** — byte ops (`LoadLocalByte`, `c6`) ⇒ `char`; word ops ⇒ `int`;
  `Cwd` + `dx:ax` arithmetic ⇒ `long`.
- **Signedness** — `Cbw`/`Cwd`/`sar`/`idiv` ⇒ signed; zero-extend / `shr`
  / `div` ⇒ unsigned.
- **Pointers** — `Lea` of a slot, or `PointerLoad`/`PointerStore` through
  `[si]/[di]`, ⇒ a pointer; near vs far from the relocation form.
- **Promotions** — `Cbw`/`Cwd` become explicit `Cast` nodes, so the emitted C's
  implicit promotions recompile to the same `cbw`/`cwd`.
- **Aggregates** — a `Lea base` then indexed access ⇒ array; a constant field
  offset added before a deref ⇒ struct field. Layout is checked the only way that
  matters: recompiling and diffing.

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
