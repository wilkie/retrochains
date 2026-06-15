# Overview

This project is meant to be a harness and experimental surface to reverse engineer
the Borland C++ 2.0 compiler. The goal is to produce a working copy, written in
Rust, with a TypeScript package that wraps the WASM build of that Rust version, of
the compiler such that it produces byte-by-byte exact replicas that the original
compiler produced.

## Motivation

This is motivated by the desire to do similar reverse-engineering of programs that
were built using this compiler. The first step is to effectively understand the
compiler and its peculiarity.

## Project Layout

```
retrochains/
├── oracles/bcc/            # BCC oracle: BC2.zip (install tree; gitignored — `oracle provision bcc` or supply your own) + BC2.{sha256,md,toml}
├── oracles/msc/            # MSC oracle: MSC500.zip (gitignored — `oracle provision msc`) + MSC500.{sha256,md,toml}
├── .bc2/                   # Gitignored. Lazily unpacked from oracles/bcc/BC2.zip on first oracle use.
├── Cargo.toml              # Rust workspace manifest
├── rust-toolchain.toml     # Pinned rustup channel + components
├── rustfmt.toml
├── pnpm-workspace.yaml     # pnpm workspace definition
├── package.json            # Workspace root: shared devDeps
├── tsconfig.base.json      # Shared TS compiler options
├── eslint.config.js        # Flat ESLint config (typescript-eslint)
├── .prettierrc.json
├── crates/                 # Rust crates (cargo workspace members)
│   ├── bcc/
│   ├── tlink/
│   ├── tasm/
│   ├── obj/
│   ├── x86/
│   ├── bcc-wasm/
│   └── oracle/             # Runs the original BC2 binaries under DOSBox
├── packages/               # TypeScript packages (pnpm workspace members)
│   └── bcc/                # @retrochains/bcc — WASM wrapper
└── specs/                  # Specs and design notes (this directory)
    ├── OVERVIEW.md
    └── RUNNING_BCC.md
```

### Rust crates

The Rust side uses a cargo workspace. The workspace is split into one crate per
original Borland tool plus two shared support crates, so that each tool can be
driven independently from the CLI or composed together via the WASM facade.

- **`bcc`** — The C/C++ compiler driver and front-end. Reimplements `BCC.EXE`,
  including the command-line surface (see `RUNNING_BCC.md`), preprocessor, lexer,
  parser, semantic analysis, and code generation. Builds a `bcc` binary and a
  library crate so the pipeline can be embedded by `bcc-wasm` and tests.
- **`tlink`** — Turbo Link 4.0 reimplementation (`TLINK.EXE`). Consumes the OMF
  object files and libraries produced by the Borland toolchain and emits DOS MZ
  executables (and, eventually, the NewExe images required by the `/Tw*` Windows
  targets).
- **`tasm`** — Turbo Assembler reimplementation (`TASM.EXE`). Parses the MASM-
  flavored x86 assembly that `bcc -S` emits (and human-written `.ASM`) and
  produces OMF object files.
- **`obj`** — Shared library for reading and writing the Intel/Microsoft OMF
  (Object Module Format) records that BCC and TASM produce and TLINK consumes.
  Used by all three tool crates.
- **`x86`** — Shared library for x86 (8086/80186/80286) instruction encoding and
  16-bit real-mode addressing. Used by both `bcc` (back-end codegen) and `tasm`
  (assembly emission).
- **`bcc-wasm`** — A `cdylib` crate that re-exports the three tools through a
  single WASM module. Its job is to expose a stable C-ABI surface that
  `@retrochains/bcc` (the TS package) can call so the entire compile / assemble
  / link pipeline runs in-browser or in Node without shelling out.
- **`oracle`** — Runs the original Borland binaries from `BC2.zip` under
  DOSBox so the rest of the workspace can diff its output byte-for-byte
  against the reference. Lazily unpacks `BC2.zip` to `.bc2/` on first use.
  See [`specs/RUNNING_BCC.md`](RUNNING_BCC.md) for the wrapper's design and
  current quirks.

The Rust build will use a cargo workspace, rustup to manage the versions, and any
standard linting, testing, and layout best practice.

### TypeScript packages

- **`@retrochains/bcc`** (in `packages/bcc/`) — TypeScript wrapper around the
  WASM build of `bcc-wasm`. This is the user-facing package. It mirrors the
  original toolchain so callers can invoke `bcc`, `tlink`, and `tasm` with
  the same CLI surface the original binaries expose.

The TypeScript and JavaScript ecosystem will consist of pnpm, eslint, prettier,
and use any best practice to structure its build system and testing practices.

## The oracle

The original Borland C++ 2.0 binaries (`BCC.EXE`, `TASM.EXE`, `TLINK.EXE`)
along with their headers and runtime libraries are supplied as
`oracles/bcc/BC2.zip`. The Borland binaries aren't ours to redistribute, so
`BC2.zip` is gitignored; we track the [`BC2.sha256`](../oracles/bcc/BC2.sha256)
integrity manifest and the [`BC2.md`](../oracles/bcc/BC2.md) how-to-acquire doc
instead (and `oracle provision bcc` rebuilds the zip from them — see
[`PROVISIONING.md`](PROVISIONING.md); same pattern as `MSC500.{sha256,md}`). On first use, `crates/oracle/` unpacks the archive into a
gitignored `.bc2/` directory and from then on drives those binaries under DOSBox
to produce reference outputs.

That reference is the **oracle**: every byte our Rust reimplementation
produces must match the byte the oracle produces for the same input. See
[`RUNNING_BCC.md`](RUNNING_BCC.md) for how to invoke it, the design of the
DOSBox wrapper, and the open issues (notably timestamp determinism).
