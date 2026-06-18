# Decompiler: read-modify-write to memory operands

A plan to teach the decompiler the **in-place compound update of a memory
operand** — `add/sub/and/or/xor/adc/sbb [mem], reg|imm`, `inc/dec [mem]` — the
single biggest lever on the `incomplete` bucket (see
[[decompiler_incomplete_triage]] / `examples/triage.rs`). Today the lift only
decodes *register*-destination ALU, so every memory-destination read-modify-write
(RMW) becomes an opaque `Asm` run and the function bails.

## Why this, why now

Current decompiler sweep (`cargo run -p decompile --example sweep`): **2159 MATCH
(52.2%) / 1673 incomplete (40.4%) / 283 mismatch**. The incomplete bucket
clusters (by `Function::bail_reason`):

- `Asm(unlifted)` 641 (38%) — led by `01 add [mem],reg` (96), `81 grp1 [mem],imm`
  (90), `fe inc/dec [mem8]` (53). These are exactly mem-dest RMW the lift drops.
- `Bin:Adc` 14 / `Bin:Sbb` 6 — the `long` RMW halves.
- By area: compound-assign (209) is the #1 incomplete area; struct-field /
  array-element / `long` compound is scattered through bitwise (142) and
  arithmetic (102) with the same root cause.

Plausible reach: 250–400 fixtures. The work is an **extension of existing
machinery**, not a new concept — `Stmt::Compound` already recovers reg-var,
local, and even-global RMW (`add si,5`, `add [g],3`). And the forward direction
is already byte-exact: our `bcc` emits these mem-dest instructions (the fixtures
sit in the green 4129-pool), so a correctly-recovered `*p += K` recompiles to the
same bytes. We are only teaching the *reverse*.

## What already works (the baseline to extend)

- Lift idiom `AluImm` (`0x83`, ALU r/m16 **imm8-sign-extended**) →
  `lo_ir::decode` (`lo_ir.rs:468`) emits `Bin { dst: place, op, lhs: place, rhs:
  Imm }` for `[bp+disp8]`, `[disp16]` (global), `[si]/[di]` deref, and register.
- Recover arm `hi_ir.rs:1958`: a `Bin { dst, op, lhs, rhs } if dst == lhs &&
  is_compound_op(op) && var_of(dst).is_some()` → `Stmt::Compound(lv, op, rhs)`.
  `var_of` (`hi_ir.rs`) maps `Local(<0)`→Slot, `Local(>=4)`→Param, reg-var,
  **even** `Global`, byte-reg-var → `Var`; everything else (deref, odd/interior
  global) → `None`, so the arm declines.
- Emit: `compound_str` renders `++`/`--` for `±1` and `lv op= rhs` otherwise;
  `lvalue_str` already spells `LValue::Deref` (`*p`, `a[i]`). So the emitter needs
  no change for the cases below — verify, don't rewrite.

So `g += 7` (global, imm8) already round-trips. The gaps are (a) other *opcodes*
in the lift, and (b) *deref destinations* in the recovery.

## The gap inventory

Lift (add idiom templates in `crates/fingerprint/src/idioms.rs` + decode arms in
`crates/decompile/src/lo_ir.rs`):

1. **`0x81` — ALU r/m16, imm16** (full-word immediate). Same operand shapes as the
   existing `0x83` `AluImm`, but the immediate is 2 bytes, not 1 sign-extended
   byte. ~90 fixtures (large constants; bitfield `and/or/sub` masks).
2. **`0x01/0x09/0x21/0x29/0x31/0x39` — ALU r/m16, r16 mem-dest** (`op [mem],
   reg`). The mirror of the reg-dest `0x03/0x0b/...` (`AluReg/AluLocal/AluGlobal/
   AluDeref`). ~96 fixtures (`x op= y`, variable RHS).
3. **`0xfe /0,/1 — inc/dec byte [mem]`**. `IncDecByteReg` only covers the register
   form; need the memory form. ~53 fixtures (char / bitfield byte bumps). (The
   **word** `0xff /0,/1 [mem]` already decodes to `Un{Inc/Dec}` via
   `Grp5Local/Grp5Global` and recovers through the `Un` arm at `hi_ir.rs:1940` —
   for local/global; the deref form rides on Stage 3.)
4. **`long` RMW** — `adc/sbb [mem]` (the high-word half) paired with the low-word
   `add/sub [mem]`, and `0x81 /2,/3` (`adc/sbb [mem], imm`). Recovers as one
   `long` compound, mirroring the existing `long` store-pair handling. ~20.

Recover (`crates/decompile/src/hi_ir.rs`):

5. **Deref destinations.** Extend the Compound recovery so a `Bin { dst, op, lhs,
   rhs } if dst == lhs` with `dst ∈ {Deref(r), DerefDisp(r, d)}` recovers
   `Stmt::Compound(LValue::Deref(ptr_expr), op, rhs)`, resolving `ptr_expr` from
   the tracked base register (the same `bx`/pointer machinery the `*p = v` store
   arm at `hi_ir.rs:1988` uses). This is what unlocks `*p += K`, `p->f += K`
   (struct field via pointer), and `a[i] += K` (array element). Highest value;
   most of the struct/array compound count lives here.
6. **Odd / interior globals.** `var_of` declines odd `Global` offsets (`char`
   globals, struct/array interiors). Out of scope for v1 unless cheap — note and
   defer; it overlaps the globals-layout work.

## Staging — cheapest/safest first, gated by the sweep each step

Each stage: add the lift template(s) + decode, extend recovery if needed, add
unit round-trip tests in the `emit`/`hi_ir` test modules, then run
`cargo run -p decompile --example sweep` and require **MATCH strictly up, mismatch
not up** before moving on. The `examples/triage.rs` clusters and the explorer's
reason chips are the live read-out (watch `Asm(unlifted)` / `Bin:Adc` shrink).

- **Stage 1 — `0x81` imm16 word compound (lift only).** Recovery already handles
  Local/Global/Reg dst, so this is a pure lift extension mirroring `AluImm`.
  Lowest risk; unblocks the imm16 cluster. *Acceptance:* `g &= 0xFF00;`,
  `x -= 1000;` (local) round-trip; MATCH up.
- **Stage 2 — `op [mem], reg` mem-dest ALU (lift only)** for local/global dst.
  Mirror `AluLocal/AluGlobal` with the destination as the r/m place. *Acceptance:*
  `g += y;`, `x ^= y;` (local/global, variable RHS) round-trip.
- **Stage 3 — deref destinations (lift + recover).** The recovery extension (#5)
  plus the deref operand shapes for the Stage-1/2 opcodes. Unlocks `*p += K`,
  `p->f op= y`, `a[i] += K`. Highest value; do struct-field and array-element
  fixtures together since they share the deref path. *Acceptance:* a struct-field
  compound (`aggregates/struct/3443-member-pluseq`) and an array-element compound
  (`arrays/indexing/1210-int-array-elem-compound-mul`) round-trip.
- **Stage 4 — byte mem inc/dec + `long` RMW.** `0xfe [mem]` and the `adc/sbb`
  long-pair (#3, #4). Closes the char and `long` compound tails.

## Risks & guards

- **In-place vs load-op-store.** `x += K` (one mem instruction) and `x = x + K`
  (load, op, store-back) are *different bytes*; the former is `Stmt::Compound`,
  the latter `Stmt::Assign`. The existing `dst == lhs` guard already separates
  them for registers — preserve that invariant when extending to deref. A
  mis-recovery shows up immediately as a sweep **mismatch**, not a silent error.
- **±K asymmetries.** The recompile must reproduce BCC's exact idiom
  (`+1/+2`→inc, `-1`→dec, `-2`→`add -2`, pointer stride doubling) —
  see [[bcc_immediate_asymmetries]]. We recover the *same* op the lift saw and
  emit `x += K` / `x++`; our `bcc` already chooses the byte-exact form (green
  pool), so the verifier confirms. No new idiom-selection logic in the decompiler.
- **No regressions.** Decompiler-only change; the byte-exact `xfix` pools are
  untouched. The gate is the sweep: MATCH must climb and the 283-mismatch bucket
  must not grow. Run it after every stage.
- **Boundary with globals.** Some compound-to-global fixtures decline at
  `program:globals` (the emitter's data-segment-layout gap), *not* the lift — so
  they won't flip on this work alone. That's the separate, cheaper follow-up;
  don't conflate the counts.

## Acceptance / done

- Sweep MATCH up by a meaningful margin (target ≥ +200), `Asm(unlifted)` and
  `Bin:Adc`/`Bin:Sbb` clusters substantially shrunk in `examples/triage.rs`.
- New round-trip tests in `crates/decompile` covering each stage's shapes (the
  `assert_roundtrips` harness — compile, decompile, recompile byte-exact).
- No mismatch-bucket growth; both `xfix` pools still green (unaffected, but
  confirm).
- Update [[decompiler_incomplete_triage]] with the new numbers.

## Pointers

- Lift idioms: `crates/fingerprint/src/idioms.rs` (`Def`/`Idiom`, the `0x83`
  `AluImm` patterns ~L460); decode: `crates/decompile/src/lo_ir.rs`
  (`AluImm`@468, `AluReg/Local/Global/Deref`@391–460, `group1_op`, `alu_op`).
- Recover: `crates/decompile/src/hi_ir.rs` (Compound arm @1958, `var_of`,
  `dest`, the `*p = v` deref-store arm @1988 for the pointer-tracking pattern).
- Emit (verify-only): `crates/decompile/src/emit.rs` (`compound_str`,
  `lvalue_str`).
- Measure: `examples/sweep.rs`, `examples/triage.rs`, `examples/asm_gaps.rs`;
  explorer reason chips (`decompile_reasons`).
