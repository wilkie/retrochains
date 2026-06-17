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

| bucket      | initial | … | regvar-ptr | addr-of-global | ptr-arith+deref | meaning |
|-------------|--------:|---|-----------:|---------------:|----------------:|---------|
| **MATCH**   |  1433 (34.7%) | … | 2074 (50.2%) | 2103 (50.9%) | **2130 (51.5%)** | round-trips byte-exact |
| incomplete  |  2111 (51.1%) | … | 1746 | 1722 (41.7%) | 1710 (41.3%) | recovery declines (sound) — a feature gap |
| MISMATCH    |   553 (13.4%) | … | 273 | 264 (6.4%) | 275 (6.6%) | recovered C recompiles to *different* bytes |
| cerr        |     2 | … |  2 |  2 |  7 | recovered C didn't compile |
| notext      |     5 | … |  5 |  5 |  5 | no `_TEXT` (all-data fixture; nothing to recover) |
| PANIC       |    27 | … | 26 | 30 |  9 | recover/verify crashed |

(The pointer-arithmetic + deref step: array-decay-to-pointer recovery dissolved
most of the panics (30→9) by giving `bcc` a valid array name instead of a stray
`&v?`; int-pointer `p++`/`p += K` coalescing and `*p++` (`PostIncDeref`) and
deref-in-comparison (`cmp [si],n` / `cmp byte [si],n`, both previously
mis-decoded as raw `Asm`) added the +27 match. The cerr creep is new long-fold
fixtures from the gap loop, not a regression here.)

(`&global` was a clean conversion: +17 match **and −17 mismatch**, zero new —
every `&g`-recovered-as-`0` mismatch fixed.)

(The reg-var pointer work — offset 0 then offset-K — added +70 match across two
commits and crossed 50%. The mismatch/panic creep is adjacent gaps it unblocked:
`&global`, char/int pointer arithmetic (our `bcc` panics on some), string-via-ptr
— all production-gated by `render_idiomatic`.)

(The reg-var-pointer step crossed 50%: +58 match. The +19 mismatch / +7 panic
are adjacent gaps it unblocked — `&global` mis-recovered as `0`, char-pointer
arithmetic (`p++`) that crashes our `bcc`, bitfield-via-ptr, arrow-compound,
string-via-ptr — all production-gated by `render_idiomatic`.)

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
9. ~~**`char*char→int` / `int→long` widening**~~ — **PARTIAL** (MATCH 48.4% →
   48.6%, MISMATCH 260 → 254). `char*char` promotes via `cbw` then multiplies in
   the register spill (`imul dx` now reads the spilled right operand, not only a
   constant). `int→long` is a `cwd` *not* feeding an `idiv` → `acc_long`, so the
   widened value returns/propagates as `long`. **Still open:** the `long`-local
   *store* (`long r = (long)i` / `long r = a+b`) — the `dx:ax`→two-slot store
   pair, which uses *opposite* register→slot orderings for a widened int (`dx`
   high) vs a long add (`ax` high); a real long-store fold, deferred.
10. ~~**`long`-local store fold**~~ — **PARTIAL** (MATCH 48.6% → 48.7%). A
    `dx:ax`→two-slot store pair (`paired_long_store_low`, either register order)
    folds to one `long` assignment, so `long r = (long)i` recovers. Guarded:
    a call clears `acc_long`, so a `long` shift/mul via a runtime helper (result
    in `dx:ax` too) declines instead of folding a stale value. **Still open:** the
    *reversed* long add/load (`ax` high, used when a `long` arithmetic result is
    stored to a local rather than returned) — `long r = a + b` still declines.
11. ~~**Reversed long add/load ordering**~~ — **DONE** (MATCH 48.7% → 48.8%). The
    `ax`-high layout BCC uses for a `long` arithmetic result stored to a local:
    the reversed load (`mov ax,[hi]; mov dx,[lo]`, dropping the stray high-slot
    `int`) and the add/`adc` arms generalized to either register order. `long r =
    a + b` recovers.
12. ~~**Register-variable pointer deref**~~ — **DONE** (MATCH 48.8% → 50.2%, +58).
    BCC keeps a pointer in a reg var (si/di), so `*p` is `mov ax,[si]`, not the
    `mov bx,p; mov ax,[bx]` stack form. Added reg-var deref load/store, int and
    char width. This was the gate on `p->x` (offset-0 struct field = `*p`), and
    unblocked reg-var pointers broadly.
13. ~~**Offset-K reg-var deref**~~ — **DONE** (MATCH 50.2% → 50.5%). Added
    `mov r,[si/di+disp]` idiom patterns (rm=si `0x44` / di `0x45`, not bp `0x46`;
    `deref_base` already maps them) and the `DerefDisp(si/di)` load/store fold
    arms, int and char. `p->y` / `p[K]` / `*(p+K)` via reg-var pointers recover —
    as the byte-identical pointer form (struct fields still aren't *named*, but
    the offset is captured and round-trips).
14. ~~**`&global`**~~ — **DONE** (MATCH 50.5% → 50.9%, MISMATCH 281 → 264, −17,
    zero new). A pointer reg var (it's dereferenced) loaded with an immediate is
    `&global` at that data-segment offset, not a literal — `mov si,<offset>` is a
    fixup'd 3-byte load distinct from `xor` (null). `reg_is_dereferenced(r)` gates
    it; recovered as `Expr::AddrOf(Var::Global(off))`. *Open tail:* `&global` in a
    `return`/argument position (not an assignment), and odd-offset (char) globals.
15. **Pointer arithmetic** (`p++`, `p += 1`, `*p++`) — char and int; our `bcc`
    panics on some recovered forms. String-via-pointer, bitfield-via-ptr.
14. **Long ⨯ aggregate** (≈150 incompletes), **panic → sound-incomplete**,
    **shared globals**, bitfields, broad struct/array.
