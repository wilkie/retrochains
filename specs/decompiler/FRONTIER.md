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

## Baseline (2026-06, 4131 considered, 70 skipped)

| bucket      | count | share | meaning |
|-------------|------:|------:|---------|
| **MATCH**   |  1433 | 34.7% | round-trips byte-exact |
| incomplete  |  2111 | 51.1% | recovery declines (sound) — a feature gap |
| MISMATCH    |   553 | 13.4% | recovered C recompiles to *different* bytes |
| cerr        |     2 |  0.0% | recovered C didn't compile |
| notext      |     5 |  0.1% | no `_TEXT` (all-data fixture; nothing to recover) |
| PANIC       |    27 |  0.7% | recover/verify crashed |

(70 skipped = link invocations or unparseable args — no single-function
`_TEXT` target.)

So single-function recovery already round-trips **a third of the corpus**.

## The #1 lever: multi-function `_TEXT`

The decompiler treats the entire `_TEXT` segment as **one function**. But ~877
fixtures (~21%) define more than one function — almost always a helper plus a
`main` that calls it. On those, `recover` runs the first function's body
straight into the second:

```
int f(int x) { return x; }
int main(void) { return f(7); }
```
recovers as
```
int f(int p1) {
  return p1;
  return g0(7);   // ← main's body, mashed into f
}
```
→ MISMATCH (20 recovered vs 25 target bytes).

This single gap is the **dominant MISMATCH cause**: 187 of the 553 mismatches
are in `functions/{calls,args,return,recursion}` alone, and many more across
`control-flow`/`expressions` are multi-function files that happen to exercise
those idioms. It also accounts for a slice of the incomplete bucket (any
multi-function file whose *combined* body holds an op the fold can't model).

**What it needs:** split `_TEXT` into per-function ranges (each `enter…ret`
spans a function; the publics/segment offsets name them), recover each
independently, emit them together, and resolve a recovered call's target offset
to the recovered callee's name (instead of the opaque `extern int g0()`). The
verify then gates the *whole* multi-function C against the *whole* `_TEXT`.

This is the highest-leverage item: it converts mismatch→match for the function
clusters and unblocks honest call-target naming.

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

1. **Multi-function `_TEXT` splitting** — biggest single jump in MATCH%, and a
   prerequisite for honest call recovery.
2. **Generalized binary-op fold** — probe compound-assign/bitwise/arithmetic for
   the shared root; one fold likely clears hundreds of incompletes.
3. **Panic → sound-incomplete hardening** — 27 crashes downgraded to declines.
4. Then re-sweep and re-rank; arrays/struct/pointer-deref are the next tier.
