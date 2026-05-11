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
borland-c20/
├── Cargo.toml              # Rust workspace manifest
├── rust-toolchain.toml     # Pinned rustup channel + components
├── rustfmt.toml
├── pnpm-workspace.yaml     # pnpm workspace definition
├── package.json            # Workspace root: shared devDeps, oracle dep
├── tsconfig.base.json      # Shared TS compiler options
├── eslint.config.js        # Flat ESLint config (typescript-eslint)
├── .prettierrc.json
├── crates/                 # Rust crates (cargo workspace members)
│   ├── bcc/
│   ├── tlink/
│   ├── tasm/
│   ├── obj/
│   ├── x86/
│   └── bcc-wasm/
├── packages/               # TypeScript packages (pnpm workspace members)
│   └── bcc/                # @borland-c20/bcc — WASM wrapper
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
  `@borland-c20/bcc` (the TS package) can call so the entire compile / assemble
  / link pipeline runs in-browser or in Node without shelling out.

The Rust build will use a cargo workspace, rustup to manage the versions, and any
standard linting, testing, and layout best practice.

### TypeScript packages

- **`@borland-c20/bcc`** (in `packages/bcc/`) — TypeScript wrapper around the
  WASM build of `bcc-wasm`. This is the user-facing package. It mirrors the
  original toolchain so callers can invoke `bcc`, `tlink`, and `tasm` the same
  way `RUNNING_BCC.md` describes for `@rawrs/borland-c-2`.

The TypeScript and JavaScript ecosystem will consist of pnpm, eslint, prettier,
and use any best practice to structure its build system and testing practices.

## Running Borland C++ 2.0

The original Borland C++ 2.0 binaries are accessible via the `@rawrs/borland-c-2`
package. One can run the `bcc`, `tlink`, and `tasm` binaries via `npm exec` from
those packages. For more information, read `RUNNING_BCC.md` which has details and
an end-to-end example of using `BCC.EXE`, `TLINK.EXE`, and `TASM.EXE`. Other
options will be discovered and outlined here or in other specs as needed.

This package is the **oracle** for correctness: every output produced by our
Rust reimplementation must match what `@rawrs/borland-c-2` produces, byte for
byte, for the same input.
