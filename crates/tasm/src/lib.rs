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
    let mut module = parse::parse(source)?;
    // TASM auto-injects `FIDRQQ` (Borland's 8087 emulator entry
    // marker) into the EXTDEF list whenever the module contains any
    // 8087 instruction, even when emulation is off. The symbol is
    // declared but never referenced by a FIXUPP — the linker uses
    // its presence to pull in the floating-point runtime. Injected
    // at the FRONT so it becomes EXTDEF index 1 and the
    // already-declared extrns shift to indices 2..N.
    if module_has_fpu(&module)
        && !module.externs.iter().any(|n| n == "FIDRQQ")
    {
        module.externs.insert(0, "FIDRQQ".to_string());
    }
    emit::emit(&module)
}

fn module_has_fpu(module: &ir::Module) -> bool {
    use ir::{Instr, SegItem};
    module.segments.iter().any(|seg| {
        seg.items.iter().any(|it| match it {
            SegItem::Instr(i) => matches!(
                i,
                Instr::FldDwordBpRel { .. }
                | Instr::FstpDwordBpRel { .. }
                | Instr::FldQwordBpRel { .. }
                | Instr::FstpQwordBpRel { .. }
                | Instr::FldDwordGroupSym { .. }
                | Instr::FldQwordGroupSym { .. }
                | Instr::FpuArithBpRel { .. }
                | Instr::Fld1
                | Instr::FsubpStack
            ),
            _ => false,
        })
    })
}
