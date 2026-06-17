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

| bucket      | initial | after multi-fn | meaning |
|-------------|--------:|---------------:|---------|
| **MATCH**   |  1433 (34.7%) | **1580 (38.2%)** | round-trips byte-exact |
| incomplete  |  2111 (51.1%) | 2129 (51.5%) | recovery declines (sound) — a feature gap |
| MISMATCH    |   553 (13.4%) | 389 (9.4%) | recovered C recompiles to *different* bytes |
| cerr        |     2 |   2 | recovered C didn't compile |
| notext      |     5 |   5 | no `_TEXT` (all-data fixture; nothing to recover) |
| PANIC       |    27 |  27 | recover/verify crashed |

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
2. **Generalized binary-op / in-place-modify fold** — probe compound-assign,
   bitwise, arithmetic, and inc-dec for the shared root (a param/local mutated in
   place reads as a copy-into-fresh-local today). One fold likely clears hundreds
   of incompletes *and* the remaining `functions/*` mismatches.
3. **Panic → sound-incomplete hardening** — 27 crashes downgraded to declines.
4. **Shared globals across functions** — lift the multi-function global decline
   (one data-segment layout reconciled across the program).
5. Then re-sweep and re-rank; arrays/struct/pointer-deref are the next tier.
