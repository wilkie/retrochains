use super::*;

/// `fld <dword|qword> ptr <operand>` — dispatch based on the width
/// prefix and operand form (`[bp+disp]` or `<group>:<sym>[+disp]`).
pub(crate) fn parse_fld(rest: &str, line_no: usize) -> AsmResult<Instr> {
    if let Some(offset) = parse_dword_bp_relative(rest) {
        return Ok(Instr::FldDwordBpRel { offset });
    }
    if let Some(offset) = parse_qword_bp_relative(rest) {
        return Ok(Instr::FldQwordBpRel { offset });
    }
    if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp_width(rest, "dword") {
        return Ok(Instr::FldDwordGroupSymBx {
            group: group.to_string(),
            symbol: symbol.to_string(),
            disp: disp as i16,
        });
    }
    if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp_width(rest, "qword") {
        return Ok(Instr::FldQwordGroupSymBx {
            group: group.to_string(),
            symbol: symbol.to_string(),
            disp: disp as i16,
        });
    }
    if let Some((group, symbol)) = parse_group_symbol_with_width(rest, "dword ptr ") {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::FldDwordGroupSym {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    if let Some((group, symbol)) = parse_group_symbol_with_width(rest, "qword ptr ") {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::FldQwordGroupSym {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    Err(AsmError::new(
        line_no,
        format!("fld: unsupported operand form `{rest}`"),
    ))
}
/// `fstp <dword|qword> ptr [bp+disp]` — only bp-relative store
/// forms are emitted by BCC for float/double locals; group-symbol
/// stores would land in `fst` territory.
pub(crate) fn parse_fstp(rest: &str, line_no: usize) -> AsmResult<Instr> {
    if let Some(offset) = parse_dword_bp_relative(rest) {
        return Ok(Instr::FstpDwordBpRel { offset });
    }
    if let Some(offset) = parse_qword_bp_relative(rest) {
        return Ok(Instr::FstpQwordBpRel { offset });
    }
    if let Some((group, symbol)) = parse_group_symbol_with_width(rest, "dword ptr ") {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::FstpDwordGroupSym {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    if let Some((group, symbol)) = parse_group_symbol_with_width(rest, "qword ptr ") {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::FstpQwordGroupSym {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    Err(AsmError::new(
        line_no,
        format!("fstp: unsupported operand form `{rest}`"),
    ))
}
/// `<fadd|fsub|fmul|fdiv> <dword|qword> ptr [bp+disp]` — 8087
/// arithmetic between the FPU top and a stack-resident operand.
/// Width and op are picked by the caller (the dispatch in
/// `parse_instr`); this helper just consumes the operand. The
/// no-operand register forms (`fsub` alone = `fsubp st(1),st`)
/// land somewhere else.
pub(crate) fn parse_fpu_arith(
    rest: &str,
    op: FpuArithOp,
    mnemonic: &str,
    line_no: usize,
) -> AsmResult<Instr> {
    if let Some(offset) = parse_dword_bp_relative(rest) {
        return Ok(Instr::FpuArithBpRel { op, width: FpuWidth::Dword, offset });
    }
    if let Some(offset) = parse_qword_bp_relative(rest) {
        return Ok(Instr::FpuArithBpRel { op, width: FpuWidth::Qword, offset });
    }
    if let Some((group, symbol)) = parse_group_symbol_with_width(rest, "dword ptr ") {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::FpuArithGroupSym {
            op,
            width: FpuWidth::Dword,
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    if let Some((group, symbol)) = parse_group_symbol_with_width(rest, "qword ptr ") {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::FpuArithGroupSym {
            op,
            width: FpuWidth::Qword,
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    Err(AsmError::new(
        line_no,
        format!("{mnemonic}: unsupported operand form `{rest}`"),
    ))
}
