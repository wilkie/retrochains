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
subdirectory. The order roughly follows the compilation pipeline:

- [`bcc/`](bcc/) — the C/C++ compiler (`BCC.EXE`). Command-line surface,
  preprocessor behavior, lexer/parser quirks, codegen patterns, optimizer
  effects per flag, the `.ASM` dialect it emits.
- [`tasm/`](tasm/) — the assembler (`TASM.EXE`). The asm dialect it accepts
  (mostly what `bcc -S` emits, plus what hand-written `.ASM` looks like) and
  the OMF it produces.
- [`tlink/`](tlink/) — the linker (`TLINK.EXE`). The OMF records it
  consumes, segment ordering, fixups, and the MZ executable (and
  eventually NewExe) it emits.

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
