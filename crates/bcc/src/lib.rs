//! Borland C++ 2.0 compiler reimplementation. The library exposes the
//! compiler pipeline (lex, parse, sema, codegen) so it can be embedded by
//! the CLI binary and by the WASM wrapper. The CLI surface and the bytes
//! emitted must match `BCC.EXE` exactly; see `specs/RUNNING_BCC.md` and
//! `specs/bcc/ASM_OUTPUT.md`.

mod cli;
mod dos_time;
mod emit_s;

pub use cli::{CliError, CompileMode, MemoryModel, ParsedArgs, parse_args};
pub use emit_s::{EmitError, build_asm, emit_dash_s};
