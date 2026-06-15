# TASM — the assembler

Discoveries about `TASM.EXE` go here. The actual Rust reimplementation
lives in `crates/bcc-tasm/` (crate `bcc-tasm`, imported as `bcc_tasm`),
with the public API `bcc_tasm::assemble(asm_text) -> Result<Vec<u8>, AsmError>`.

The `bcc -c` pipeline goes: source → `build_asm` (text) → `bcc_tasm::assemble`
(OMF bytes). Every byte-exactness fix that lands for `-S` propagates
into `-c` automatically.

## What we've pinned

- **Translator string**: TASM 2.0 always injects a COMENT class 0x00
  with the length-prefixed string `TC86 Borland Turbo C++ 2.0` right
  after THEADR. See [`../formats/OMF.md`](../formats/OMF.md) §COMENT.
- **Memory-model marker**: A COMENT class 0xEA with payload `01 09` is
  injected just before LNAMES for small model (`-ms`). The byte
  meanings aren't fully pinned; non-small-model fixtures will widen
  this.
- **Trailing module record**: A COMENT class 0xE8 with payload
  `00 <name-len> <name> <packed-mtime>` is injected just before
  MODEND. The mtime is read out of the earlier `?debug C E9...`
  record.
- **FIXUP ordering is LIFO**. Within a single FIXUPP record, fixups
  appear in the reverse of the order their instructions were encoded.
  Fixture 108 disambiguates: the string-pointer load (data offset 8)
  is encoded *before* the printf call (data offset 12), but the
  captured FIXUPP has the call entry first. The simplest explanation
  is a stack-based fixup collection inside TASM.
- **THREAD subrecords are never used**. Every FIXUP is fully explicit.
  Another BCC/TASM 2.0 fingerprint.

## What still needs pinning

- `?debug L <line>` records (not seen yet — debug-mode flag
  combination may differ from our oracle's).
- TYPDEF records (typedef-bearing source).
- LIDATA (iterated data) vs. LEDATA — does TASM choose LIDATA for big
  zero-init globals? No fixture yet.
- Non-small memory models: macro preamble shape, COMENT 0xEA bytes,
  segment ordering.

## Suggested follow-on files

- `INPUT_DIALECT.md` — the exact subset of MASM syntax our
  `crates/bcc-tasm` accepts, and where it diverges from Borland's full
  TASM 2.0.
- `OMF_EMISSION.md` — TASM directive → OMF record table, with fixture
  citations.
- `MACROS.md` — macro language details, once a fixture forces them.

Always link discoveries back to the fixture that demonstrates them.
