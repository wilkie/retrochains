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
    // TASM auto-injects `FIWRQQ` and `FIDRQQ` (Borland's 8087
    // floating-point markers) when the module uses standalone
    // fwait / 8087 instructions. BCC's EXTDEF ordering placement
    // depends on what other externs are in the module: user-
    // function externs (e.g. `_printf`) come BEFORE the markers,
    // while runtime helpers (e.g. `N_FTOL@`) come AFTER. Tasm
    // can't distinguish these from the asm source alone, so the
    // BCC codegen now positions the markers explicitly in the asm
    // (between user externs and helpers); we just append at the
    // end here as a safety net for modules that didn't pre-emit.
    if module_has_fwait(&module)
        && !module.externs.iter().any(|n| n == "FIWRQQ")
    {
        module.externs.push("FIWRQQ".to_string());
    }
    if module_has_fpu(&module)
        && !module.externs.iter().any(|n| n == "FIDRQQ")
    {
        module.externs.push("FIDRQQ".to_string());
    }
    emit::emit(&module)
}

fn module_has_fwait(module: &ir::Module) -> bool {
    use ir::{Instr, SegItem};
    module.segments.iter().any(|seg| {
        seg.items.iter().any(|it| matches!(it, SegItem::Instr(Instr::Fwait)))
    })
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
                | Instr::FstpDwordGroupSym { .. }
                | Instr::FstpQwordGroupSym { .. }
                | Instr::FpuArithBpRel { .. }
                | Instr::FpuArithGroupSym { .. }
                | Instr::Fld1
                | Instr::Fchs
                | Instr::FsubpStack
                | Instr::FildWordBpRel { .. }
                | Instr::FcompBpRel { .. }
                | Instr::FcompGroupSym { .. }
                | Instr::FstswWordBpRel { .. }
                | Instr::Fwait
            ),
            _ => false,
        })
    })
}
