# Specs

Start here. Everything we know about the Borland C++ 2.0 toolchain that
isn't recoverable from reading `crates/` lives in this directory. Treat
these documents as the source of truth — when code disagrees with a spec,
update one of them deliberately.

## Top-level docs

- [`OVERVIEW.md`](OVERVIEW.md) — what this project is, project layout, the
  crates and packages.
- [`RUNNING_BCC.md`](RUNNING_BCC.md) — how the oracle (`crates/oracle/`)
  drives the original BCC/TASM/TLINK binaries under DOSBox, and the clock-
  pinning mechanism that gives us byte-exact reproducibility.
- [`PROVISIONING.md`](PROVISIONING.md) — how `oracle provision <bcc|msc>`
  rebuilds the gitignored compiler archives from public install media and
  verifies every file against the recorded `sha256` manifest.
- [`FIXTURES.md`](FIXTURES.md) — the fixture corpus layout and the
  capture/verify harness contract.
- [`GLOSSARY.md`](GLOSSARY.md) — definitions for terms used across
  the specs and code (peephole, fingerprint, slot, segment override,
  etc.). Skim it once; refer back when a spec uses a term that
  isn't clicking.
- [`OPEN_QUESTIONS.md`](OPEN_QUESTIONS.md) — byte-exactness blockers we
  can't yet predict from rules. Each entry records the smallest input that
  exposes the gap, what we've ruled out, and the next investigation to
  close it. Add new entries when probing exposes behavior we don't
  understand.

## Per-tool discoveries

As we reverse-engineer each tool, observations land in the matching
subdirectory. The Borland C++ 2.0 **toolchain** specs all live under
[`bcc/`](bcc/) — the compiler at its root, the auxiliary tools in their own
subdirectories — following the compilation pipeline:

- [`bcc/`](bcc/) — the C/C++ compiler (`BCC.EXE`). Command-line surface,
  preprocessor behavior, lexer/parser quirks, codegen patterns, optimizer
  effects per flag, the `.ASM` dialect it emits.
- [`bcc/tasm/`](bcc/tasm/) — the assembler (`TASM.EXE`). The asm dialect it
  accepts (mostly what `bcc -S` emits, plus what hand-written `.ASM` looks like)
  and the OMF it produces.
- [`bcc/tlink/`](bcc/tlink/) — the linker (`TLINK.EXE`). The OMF records it
  consumes, segment ordering, fixups, and the MZ executable (and
  eventually NewExe) it emits.
- [`bcc/tlib/`](bcc/tlib/) — the librarian (`TLIB.EXE`). The `.LIB` archive
  format and its dictionary hashing.
- [`linkers/`](linkers/) — **cross-linker** discoveries. `DIFFERENCES.md`
  catalogs Borland TLINK vs Microsoft LINK MZ output — the EXE-level
  toolchain fingerprints feeding compiler-aware decompilation.

## Analysis and decompilation

The inverse direction — reading binaries *back* into source. Where the
toolchain specs describe how bytes are produced, these describe how we
recover what produced them.

- [`FINGERPRINTS.md`](FINGERPRINTS.md) / [`MSC_FINGERPRINTS.md`](MSC_FINGERPRINTS.md)
  — how `crates/fingerprint/` decides *which* compiler (and which toolchain
  details) produced an OBJ/LIB/EXE, from both symbol/structure markers and
  codegen idioms.
- [`decompiler/`](decompiler/) — turning recognized code back into C.
  [`IR.md`](decompiler/IR.md) specifies the intermediate representation the
  decompiler lifts machine code into and emits compiler-accurate C from,
  verified by recompiling with our byte-exact `bcc`.

## Shared formats

[`formats/`](formats/) — file-format references that more than one tool
touches. OMF (object file format), MZ executable, library archive (`.LIB`),
debug-info format, etc.

## How to add a spec

1. Land discoveries in the subdirectory that fits — by tool if it's
   tool-specific, by format if it's about a file format more than one tool
   touches.
2. Prefer many small files over one growing file. A spec doc should answer
   one question; cross-link between them.
3. Update the relevant `README.md` index when you add a new file.
4. Cite evidence: which fixture demonstrates the behavior, which oracle
   invocation reproduces it. "I observed" beats "I think".
