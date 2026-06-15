use super::*;

/// `jmp` covers two forms BCC emits in -ms: `jmp short <label>`
/// (intra-segment near jump, 2 bytes) and `jmp word ptr cs:<sym>[bx]`
/// (jump-table dispatch, 5 bytes + FIXUPP — fixture 158).
pub(crate) fn parse_jmp(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let s = operands.trim();
    if let Some(rest) = s.strip_prefix("short").map(str::trim_start) {
        return Ok(Instr::JmpShort(rest.to_string()));
    }
    if let Some(rest) = s.strip_prefix("word ptr cs:").map(str::trim_start) {
        // Two forms:
        //   1. `<symbol>[bx]` — jump-table base + index (fixture 158).
        //   2. `[bx+<imm>]` — bare BX + disp8 (linear-search dispatch
        //      where BX walks into adjacent value+label tables; the
        //      disp lands at the start of the label table).
        if let Some(disp_str) = rest.strip_prefix("[bx+").and_then(|s| s.strip_suffix(']')) {
            let disp = disp_str.trim().parse::<u8>().map_err(|_| {
                AsmError::new(
                    line_no,
                    format!("jmp cs:[bx+?]: bad displacement `{disp_str}`"),
                )
            })?;
            return Ok(Instr::JmpIndirectCsBxDisp { disp });
        }
        if let Some(table) = rest.strip_suffix("[bx]") {
            return Ok(Instr::JmpIndirectCsTableBx {
                table: table.trim().to_string(),
            });
        }
        return Err(AsmError::new(
            line_no,
            format!("jmp cs: unsupported operand `{rest}`"),
        ));
    }
    Err(AsmError::new(
        line_no,
        format!("jmp: unsupported operand form `{operands}`"),
    ))
}
/// `shl <reg16>,1` — D1 /4. The 1-bit shift form used by BCC to
/// double a word-array index (`shl bx,1`). For multi-bit shifts the
/// count goes through CL (see `ShlAxCl`).
pub(crate) fn parse_shl_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("shl: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if rhs != "1" {
        // 80186+ multi-bit shift form: `shl <reg16>, <imm8>` with
        // imm in [2, 31] (a C shift count can validly be 0..=31 for
        // i16). Encoded `C1 /4 ib`, 3 bytes.
        if let Some(reg) = Reg16::parse(lhs)
            && let Ok(imm) = rhs.parse::<u8>()
            && imm >= 2
            && imm <= 31
        {
            return Ok(Instr::ShlReg16Imm8 { reg, imm });
        }
        return Err(AsmError::new(
            line_no,
            format!("shl: only `<reg>,1`, `<reg16>,<imm8>`, and `ax,cl` forms supported (got `{rhs}`)"),
        ));
    }
    // 8-bit form first (fixture 535: `shl dl,1`).
    if let Some(reg) = Reg8::parse(lhs) {
        return Ok(Instr::ShlReg8One { reg });
    }
    // `shl word ptr <group>:<sym>[+N], 1` — memory-direct on a
    // global (fixture 539).
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::ShlGroupSymOne {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    // `shl byte ptr <group>:<sym>[+N], 1` — byte memory-direct on
    // a global (fixture 688: `g <<= 2` unrolls to two such).
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::ShlGroupSymByteOne {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    // `shl word ptr [bx+disp8], 1` — pointer-subscript shift
    // (fixture 878: `int *p; p[K] <<= N` unrolls into N of these).
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::ShlBxDispImm1 { disp });
    }
    let reg = Reg16::parse(lhs)
        .ok_or_else(|| AsmError::new(line_no, format!("shl: bad register `{lhs}`")))?;
    Ok(Instr::ShlReg16One { reg })
}
/// `rcl <reg16>,1` — D1 /2. Rotate-left-through-carry by one.
/// Companion to `shl reg,1` for long left-shift-by-one (fixture 227).
pub(crate) fn parse_rcl_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("rcl: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if rhs != "1" {
        return Err(AsmError::new(
            line_no,
            format!("rcl: only `<reg>,1` form supported (got `{rhs}`)"),
        ));
    }
    let reg = Reg16::parse(lhs)
        .ok_or_else(|| AsmError::new(line_no, format!("rcl: bad register `{lhs}`")))?;
    Ok(Instr::RclReg16One { reg })
}
/// `sar <reg16>,1` — D1 /7. Arithmetic shift right by one (high-half
/// op for signed long right-shift-by-one, fixture 229).
pub(crate) fn parse_sar_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("sar: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if rhs != "1" {
        return Err(AsmError::new(
            line_no,
            format!("sar: only `<reg>,1` and `ax,cl` forms supported (got `{rhs}`)"),
        ));
    }
    if let Some(reg) = Reg8::parse(lhs) {
        return Ok(Instr::SarReg8One { reg });
    }
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::SarGroupSymOne {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::SarGroupSymByteOne {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    // `sar word ptr [bx+disp8], 1` — pointer-subscript signed
    // shift sibling.
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::SarBxDispImm1 { disp });
    }
    let reg = Reg16::parse(lhs)
        .ok_or_else(|| AsmError::new(line_no, format!("sar: bad register `{lhs}`")))?;
    Ok(Instr::SarReg16One { reg })
}
/// `shr <reg16>,1` — D1 /5. Logical shift right by one (high-half
/// op for unsigned long right-shift-by-one, fixture 243).
pub(crate) fn parse_shr_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("shr: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if rhs != "1" {
        return Err(AsmError::new(
            line_no,
            format!("shr: only `<reg>,1` and `ax,cl` forms supported (got `{rhs}`)"),
        ));
    }
    if let Some(reg) = Reg8::parse(lhs) {
        return Ok(Instr::ShrReg8One { reg });
    }
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::ShrGroupSymOne {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::ShrGroupSymByteOne {
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    // `shr word ptr [bx+disp8], 1` — pointer-subscript unsigned
    // shift sibling.
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::ShrBxDispImm1 { disp });
    }
    let reg = Reg16::parse(lhs)
        .ok_or_else(|| AsmError::new(line_no, format!("shr: bad register `{lhs}`")))?;
    Ok(Instr::ShrReg16One { reg })
}
/// `rcr <reg16>,1` — D1 /3. Rotate-right-through-carry by one.
/// Companion to `sar reg,1` for long right-shift-by-one (fixture 229).
pub(crate) fn parse_rcr_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("rcr: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if rhs != "1" {
        return Err(AsmError::new(
            line_no,
            format!("rcr: only `<reg>,1` form supported (got `{rhs}`)"),
        ));
    }
    let reg = Reg16::parse(lhs)
        .ok_or_else(|| AsmError::new(line_no, format!("rcr: bad register `{lhs}`")))?;
    Ok(Instr::RcrReg16One { reg })
}
/// `lea <reg16>,word ptr [bp+N]` — load effective address. Currently
/// only the bp-relative source form is recognized; other addressing
/// modes will land with more fixtures.
pub(crate) fn parse_lea(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("lea: expected `lhs,rhs`, got {operands:?}"))
    })?;
    let dst = Reg16::parse(lhs)
        .ok_or_else(|| AsmError::new(line_no, format!("lea: bad dst `{lhs}`")))?;
    let offset = parse_bp_relative(rhs)
        .ok_or_else(|| AsmError::new(line_no, format!("lea: unsupported source `{rhs}`")))?;
    Ok(Instr::LeaReg16BpRel { dst, offset })
}
pub(crate) fn parse_jmp_cond(kw: &str, operands: &str, line_no: usize) -> AsmResult<Instr> {
    let cond = match kw {
        "je" => JmpCond::E,
        "jne" => JmpCond::Ne,
        "jl" => JmpCond::L,
        "jle" => JmpCond::Le,
        "jg" => JmpCond::G,
        "jge" => JmpCond::Ge,
        "ja" => JmpCond::A,
        "jae" | "jnc" => JmpCond::Ae,
        "jb" | "jc" => JmpCond::B,
        "jbe" => JmpCond::Be,
        "js" => JmpCond::S,
        "jns" => JmpCond::Ns,
        "jp" => JmpCond::P,
        "jnp" => JmpCond::Np,
        "jo" => JmpCond::O,
        "jno" => JmpCond::No,
        _ => unreachable!("caller restricted the keyword"),
    };
    // `short <label>` — strip the optional `short` prefix.
    let target = operands
        .strip_prefix("short")
        .map(str::trim_start)
        .unwrap_or(operands)
        .trim()
        .to_string();
    if target.is_empty() {
        return Err(AsmError::new(line_no, format!("{kw}: missing target label")));
    }
    Ok(Instr::JmpCondShort { cond, target })
}
