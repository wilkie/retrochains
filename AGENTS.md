# AGENTS.md

Orientation for AI coding agents working in this repository.

## What this project is

A reverse-engineered reimplementation of the Borland C++ 2.0 toolchain
(`BCC.EXE`, `TLINK.EXE`, `TASM.EXE`) in Rust, packaged as WASM and exposed
through a TypeScript wrapper.

**Read these first:**

- [`specs/OVERVIEW.md`](specs/OVERVIEW.md) — project goals, motivation, full
  directory layout, and a description of every crate and package.
- [`specs/RUNNING_BCC.md`](specs/RUNNING_BCC.md) — how to invoke the original
  Borland tools (the oracle), discovered default flags, and an end-to-end
  example of `BCC` → `TLINK` (and `TASM`).

Specs are the source of truth. If you make a design decision that's not in
them, propose adding it to `specs/` rather than letting it live only in code
comments or commit messages.

## The byte-exact invariant

Every artifact our toolchain produces (object files, `.ASM` listings, `.MAP`
files, executables) **must match the output of the original Borland C++ 2.0
tools byte for byte** for the same input and flags. This is the only success
criterion that matters; performance, ergonomics, and code clarity are
secondary.

The original binaries (`BCC.EXE`, `TASM.EXE`, `TLINK.EXE`) plus headers and
runtime libraries are shipped in `BC2.zip` at the repo root. The
`crates/oracle/` crate unpacks it lazily into `.bc2/` (gitignored) and runs
the tools under DOSBox. Use it; don't shell out to DOSBox directly. Full
details and a known-issues list are in
[`specs/RUNNING_BCC.md`](specs/RUNNING_BCC.md).

```bash
# Ad-hoc oracle from the shell — compile-only, small memory model:
cargo run -q -p oracle -- bcc -ms -c -- hello.c
```

```rust
// From a Rust test:
let oracle = Oracle::open(OracleConfig::for_workspace(workspace_root))?;
let run = oracle.run(
    &OracleInvocation::new(Tool::Bcc)
        .args(["-ms", "-c", "HELLO.C"])
        .input("HELLO.C", source_bytes),
)?;
assert_eq!(run.exit_code, 0);
```

DOSBox and faketime must be installed on the host
(`sudo apt install dosbox faketime`). The wrapper runs DOSBox headlessly via
`SDL_VIDEODRIVER=dummy` and wraps the spawn in `faketime` to pin the
emulated DOS clock so BCC's embedded timestamps stay reproducible.

## Toolchains

- **Rust:** pinned by `rust-toolchain.toml` (channel 1.95, rustfmt + clippy,
  `wasm32-unknown-unknown` target). `rustup` will install it on first `cargo`
  invocation.
- **Node:** `>=22`.
- **Package manager:** **pnpm only** (declared in `package.json`'s
  `packageManager` field). Do not use `npm install` or `yarn`.

## Common commands

```bash
# Rust
cargo check --workspace
cargo build --workspace
cargo test  --workspace
cargo fmt   --all
cargo clippy --workspace --all-targets -- -D warnings

# TypeScript
pnpm install
pnpm -r run build
pnpm -r run test
pnpm -r run lint
pnpm format          # prettier --write .
pnpm format:check
```

## Repository layout (short form)

```
BC2.zip               original Borland C++ 2.0 install (tracked; the oracle source)
.bc2/                 lazily unpacked from BC2.zip (gitignored)
crates/               cargo workspace
  bcc/   tlink/  tasm/        — the three tool reimplementations (lib + bin)
  obj/   x86/                 — shared support libraries
  bcc-wasm/                   — cdylib that exposes the tools to TS over WASM
  oracle/                     — runs the BC2 binaries under DOSBox
packages/             pnpm workspace
  bcc/                        — @borland-c20/bcc, the TS wrapper
specs/                Specs and design notes (start here)
```

See `specs/OVERVIEW.md` for the full description of each crate.

## House rules for changes

- **Match the original first; refactor later.** Don't "improve" output formatting,
  whitespace, or error messages until our output already matches the oracle.
  Cosmetic divergences from `BCC.EXE` are bugs.
- **No `unsafe`** unless absolutely required (workspace lint denies it by
  default). If you need it, justify in a comment and call it out in the PR.
- **Errors:** prefer `Result` and `thiserror`/explicit enums over `panic!`.
  Panics are fine for genuinely impossible states. No `unwrap()` on user input.
- **Tests:** prefer integration tests that run our tool and the oracle on the
  same input and assert byte-for-byte equality of every output file.
- **No new top-level files without a clear reason** — extend an existing crate
  or package first.

## Things this project explicitly is *not*

- Not a portable, modernized C++ compiler — fidelity to Borland C++ 2.0 wins
  over standards conformance.
- Not a multi-version Borland support layer — only C++ 2.0 (1991) for now.
- Not a sandbox/emulator for running DOS binaries — we produce the binaries;
  running them is out of scope.
