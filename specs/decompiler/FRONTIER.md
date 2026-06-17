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

| bucket      | initial | multi-fn | in-place | var-shift | meaning |
|-------------|--------:|---------:|---------:|----------:|---------|
| **MATCH**   |  1433 (34.7%) | 1580 (38.2%) | 1761 (42.6%) | **1817 (44.0%)** | round-trips byte-exact |
| incomplete  |  2111 (51.1%) | 2129 | 2000 | 1940 (47.0%) | recovery declines (sound) — a feature gap |
| MISMATCH    |   553 (13.4%) | 389 | 336 | 340 (8.2%) | recovered C recompiles to *different* bytes |
| cerr        |     2 |   2 |  2 |  2 | recovered C didn't compile |
| notext      |     5 |   5 |  5 |  5 | no `_TEXT` (all-data fixture; nothing to recover) |
| PANIC       |    27 |  27 | 27 | 26 | recover/verify crashed |

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
4. **`char` in-place compound** — `a += b` on a `char` reg var is `add dl,al`
   (byte in-place). *Blocked:* the rhs through `al` can be an int's low byte
   (`c |= n`, n int → mis-typed `char`), and `char` params promote differently;
   needs the int-vs-char slot prescan and the param-promotion fix first (an
   attempt regressed 5 incomplete→mismatch and was reverted).
5. **Param promotion** — a param copied to a reg var and mutated (`x++` on a
   param) recovers as a fresh local + copy; works for `int` by luck but adds 2
   bytes for `char`. Recovering it as direct param mutation would also clean up
   the remaining `functions/*` mismatches.
6. **Panic → sound-incomplete** (26), **shared globals across functions**, then
   casts/bitfields (the var-shift mismatch tail), arrays/struct/pointer-deref.
