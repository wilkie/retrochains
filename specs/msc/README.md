# MSC — the Microsoft C 5.0 compiler

Discoveries about `CL.EXE` from MS C 5.0 go here. The corpus lives in
`fixtures/4075-…` onward, captured via the `microsoft-msc5` oracle
profile and replayed against `target/debug/msc`.

Existing docs:

- [`OMF_OUTPUT.md`](OMF_OUTPUT.md) — the OMF dialect MSC emits with
  `cl /c /AS`: record order, COMENT classes, FIXUPP threading conventions,
  CONST/DATA/TEXT layering.
- [`parser/`](parser/) — per-topic notes on the C subset our front-end
  accepts so far. Each note links to the fixture(s) that pinned the
  behavior.
- [`codegen/`](codegen/) — per-topic notes on instruction-selection
  decisions: chkstk, return-value emission, deref shapes, indexed
  loads.

Suggested future docs (create lazily as topics emerge):

- `DRIVER.md` — `/AS` vs other memory models, `/c` semantics, how `CL`
  fans out to `C1`/`C2`/`MASM`/`LINK`.
- `LEXER.md` — string-literal escapes, identifier rules.
- `SEMA.md` — implicit conversions, function-prototype handling.
- `OPTIMIZER.md` — what `/O*` changes once we go past Phase 1.

Always link discoveries back to the fixture that demonstrates them.
