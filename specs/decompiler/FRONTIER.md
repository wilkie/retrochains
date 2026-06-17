# Decompiler frontier — corpus sweep & triage

The decompiler was built increment-by-increment against hand-written probes.
This is the first measurement of where it actually stands on the **real BCC
corpus**, plus a ranked plan for what to build next.

## How to reproduce

```
cargo run -p decompile --example sweep -- [fixtures/c] [sample-per-bucket]
```

For every fixture under `fixtures/c` with a `HELLO.C` + `invocation.bcc.toml`,
the sweep parses the invocation's BCC flags (via the real `bcc::parse_args`)
into the matching `CompileOpts`, compiles the source to `_TEXT`, decompiles the
bytes back to C, and verifies the candidate recompiles to the same bytes under
the same opts. Each fixture lands in one bucket. The companion
`--example probe -- <fixture-dir>` dumps the source, recovered C, and verdict
for a single fixture.

## Baseline history (4131 considered, 70 skipped)

| bucket      | initial | … | narrowing-cast | unary-ops | ternary | meaning |
|-------------|--------:|---|---------------:|----------:|--------:|---------|
| **MATCH**   |  1433 (34.7%) | … | 1862 (45.1%) | 1908 (46.2%) | **2001 (48.4%)** | round-trips byte-exact |
| incomplete  |  2111 (51.1%) | … | 1920 | 1871 | 1844 (44.6%) | recovery declines (sound) — a feature gap |
| MISMATCH    |   553 (13.4%) | … | 323 | 326 | 260 (6.3%) | recovered C recompiles to *different* bytes |
| cerr        |     2 | … |  2 |  2 |  2 | recovered C didn't compile |
| notext      |     5 | … |  5 |  5 |  5 | no `_TEXT` (all-data fixture; nothing to recover) |
| PANIC       |    27 | … | 19 | 19 | 19 | recover/verify crashed |

(The ternary step is the biggest yet: +93 match **and −66 mismatch** — most of
those mismatches were ternaries (and the unary-step's abs cases) being
mis-structured as broken empty `if`s; recovering the conditional expression
fixes both. Only 2 new mismatches, ternary edge cases with side-effects/pointer
results, production-gated.)

(Intermediate columns multi-fn/in-place/var-shift elided; see git history. The
narrowing-cast step is a triple win: +16 match, −8 mismatch, **−7 panic** — the
panics were mixed `int`/`char` frames mis-modelled as `char` arrays feeding `bcc`
an `a[i]=a[j]` it crashed on; correct `int` typing dissolves them.)

(The +4 mismatch at the var-shift step is cast/bitfield-adjacent: folding the
shift exposes a missing narrowing — `(char)(v>>4)` drops the cast, a signed
bitfield's sign-extend shift pair surfaces. `render_idiomatic` gates these on
verify, so production still declines them; only the raw-`decompile()` sweep
counts them.)

(70 skipped = link invocations or unparseable args — no single-function
`_TEXT` target.)

## Lever #1 — multi-function `_TEXT` — **DONE**

The decompiler used to treat the entire `_TEXT` segment as **one function**, so
the ~877 fixtures (~21%) that define a helper plus a `main` ran the first
function's body straight into the second → MISMATCH. `recover_program` now
splits the segment on the prologue (an `Enter` that follows a `ret`, so the
`dec sp; dec sp` 2-byte-local idiom isn't a false boundary), recovers each window
independently with absolute offsets preserved, and resolves a *local* near call
to its callee's name (`f1` calling `f0`) — only true externals stay `g0`. See
§7 of [IR.md](IR.md).

Result: MATCH 1433 → **1580** (+147), MISMATCH 553 → 389 (−164); the
`functions/{calls,args}` mismatch cluster dropped 138 → 43. The 17 that went to
incomplete rather than match are programs touching globals (declined for now) or
with one sub-function still incomplete.

The remaining `functions/*` mismatches are a *different* gap — in-place
parameter mutation (`++c` / `x++` on a param recovers as a copy into a fresh
local, `char v1; v1 = p1; v1 = v1 + 1` — an extra slot and store). That's
lever #2 territory (the generalized binary-op / in-place-modify fold), not
multi-function.

## The incomplete bucket — ranked feature gaps (sound declines)

After multi-function, the incomplete bucket is genuine missing idioms. By
cluster (single-function share will be lower once multi-function is split out,
but the ordering is indicative):

| cluster                      | incomplete |
|------------------------------|-----------:|
| expressions/compound-assign  | 246 |
| arrays/indexing              | 216 |
| expressions/bitwise          | 205 |
| aggregates/struct            | 172 |
| pointers/deref               | 171 |
| expressions/arithmetic       | 151 |
| control-flow/loops           | 146 |
| control-flow/conditionals    |  76 |
| expressions/compare          |  73 |
| functions/calls              |  60 |
| control-flow/switch          |  47 |

`compound-assign`, `bitwise`, and `arithmetic` together (~600) are the fattest
seam and likely share a root: the fold models a narrow set of accumulator ops,
so any `op=`/`&`/`|`/`^`/`<<`/multi-term arithmetic shape it doesn't recognize
declines. Probing a handful should reveal whether one generalized binary-op fold
clears most of them.

## Smaller buckets

- **PANIC (27)** — robustness bugs (`arrays/indexing` 6, `expressions/cast` 5,
  `aggregates/struct` 3). A panic is *worse* than an incomplete: the sweep
  catches it, but a panic should be downgraded to a sound decline. Cheap,
  high-value hardening.
- **cerr (2)** — recovered C that won't compile: a structuring/emission bug
  (both `arrays/indexing`). Two concrete repros.
- **notext (5)** — all-data fixtures (`sizeof`, global-init only); no `_TEXT`.
  Not failures; arguably should be filtered from the denominator.

## Recommended order

1. ~~**Multi-function `_TEXT` splitting**~~ — **DONE** (MATCH 34.7% → 38.2%).
2. ~~**In-place compound modification**~~ — **DONE** (MATCH 38.2% → 42.6%). A
   register variable / global / loop variable updated in place (`inc si`,
   `add si,5`, `inc word [g]`, `di += si`) codes as one instruction, distinct
   from the load-op-store `x = x op y`. Recovered as `Stmt::Compound` (`x op= y`
   / `++` / `--`); added the `ff 06/0e` global inc/dec idiom (`Grp5Global`); the
   `for`-loop step/init detection and emit now accept compounds.
3. ~~**Variable shifts**~~ — **DONE** (MATCH 42.6% → 44.0%). `shl/shr/sar r,cl`
   (`d3 /r` idiom `ShiftCl`); the count loads into `cl` (tracked as the shift
   register, intercepted before the char-reg-var arm since `cl ∈ is_byte_reg_var`).
   Constant shifts were already handled (BCC unrolls them into `d1` shift-by-1s).
4. ~~**Param promotion**~~ — **DONE** (MATCH 44.0% → 44.2%, MISMATCH 340 → 331,
   −9 with no regressions). A mutated parameter is copied into a register
   variable at entry; the register *is* the parameter (the slot is never
   re-read), so `promote_params` rewrites the register back to the parameter and
   drops the copy. Guarded on the parameter appearing exactly once. Also made the
   `char` in-place `inc dl`/`dec dl` a `Compound` (it was a load-op-store
   `Assign`). Decisive for `char` param mutation (the spurious local cost a 2-byte
   frame); `int` was already matching but is now cleaner (`x++`, not `v1=p1;v1++`).
5. ~~**`char` in-place compound**~~ — **DONE** (MATCH 44.2% → 44.7%, MISMATCH
   unchanged — zero regressions). `a op= b` on a `char` reg var is an in-place
   `add dl,al`. The rhs through `al` can be an `int`'s low byte (`c |= n`); a
   word-slot prescan (a slot/global with a full-register or word-immediate
   store/load is an `int`) re-types it back from the char-marking the byte load
   does, *locally* in the compound arm (a global pass exposed unrelated cast
   narrowings). A complex `al` rhs (`c += a*b`, applied through a `dl` temp)
   declines rather than mis-attribute the temp.
6. ~~**`int→char` narrowing cast**~~ — **DONE** (MATCH 44.7% → 45.1%, MISMATCH
   331 → 323, PANIC 26 → 19; zero regressions). A byte load of a word-accessed
   slot is the low byte of an `int`, not a `char`: a word-slot pre-pass wraps it
   in `Expr::Cast(Char, …)` instead of char-marking, so `c = (char)x` reproduces
   the byte load (a plain `c = x` would word-load) and a mixed `int`/`char` frame
   is no longer mis-modelled as a `char` array (the panic source). The cast is
   dropped inside a `char` compound (`c |= n`).
7. ~~**Unary operators**~~ — **DONE** (MATCH 45.1% → 46.2%, +46). `-e` (`neg`),
   `~e` (`not`), and `!e` (the `neg; sbb ax,ax; inc ax` idiom that leaves 0/1) as
   a new `Expr::Unary` / `Expr::Not`. A bare `neg` opening `!x` is disambiguated
   by lookahead (the fold's skip-count consumes the `sbb`/`inc` tail). The +3
   mismatch is ternary/abs-adjacent (`a>0 ? a : -a`), now the most visible
   *incomplete* near this work.
8. ~~**Ternary / `? :` recovery**~~ — **DONE** (MATCH 46.2% → 48.4%, +93;
   MISMATCH 326 → 260, −66). The diamond whose both arms reduce to a value folds
   to `Expr::Ternary`, seeded into the consumer via `pending_acc`. This also
   reclaimed the unary-step's abs mismatches.
9. **`int→long`/`char*char→int` widening** (the imul-spill with `cbw` operands),
   **panic → sound-incomplete** (19), **shared globals across functions**,
   bitfields, arrays/struct/pointer-deref. Ternaries with a side-effecting arm
   (`a ? b++ : c`) or a pointer result are the small remaining tail here.
