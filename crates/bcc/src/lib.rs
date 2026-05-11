//! Borland C++ 2.0 compiler reimplementation. The library exposes the compiler
//! pipeline (lex, parse, sema, codegen) so it can be embedded by the CLI binary
//! and by the WASM wrapper. The CLI surface must match `BCC.EXE` exactly.
