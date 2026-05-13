//! Turbo Assembler reimplementation. Parses MASM-flavored x86 assembly produced
//! by BCC (and human-written .ASM) and emits OMF object files.
//!
//! The dialect we accept is **what BCC's `-S` actually emits** — not the full
//! TASM 2.0 surface. See `specs/formats/ASM.md` for the envelope and
//! `specs/bcc/ASM_OUTPUT.md` for what fills it. As more fixtures land we
//! widen what's recognized.
//!
//! Public API: [`assemble`] takes the `.ASM` text and returns the `.OBJ`
//! bytes. Errors carry a line number so failures point at the offending
//! source line.

mod emit;
mod encode;
mod ir;
mod lex;
mod parse;

pub use ir::{AsmError, AsmResult};

/// Top-level entry point. Parse one `.ASM` source, encode it, emit the
/// corresponding `.OBJ` bytes.
///
/// # Errors
/// Returns [`AsmError`] on any lex/parse/encode failure.
pub fn assemble(source: &str) -> AsmResult<Vec<u8>> {
    let module = parse::parse(source)?;
    emit::emit(&module)
}
