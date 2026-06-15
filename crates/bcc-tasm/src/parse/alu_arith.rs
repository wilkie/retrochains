use super::*;

pub(crate) fn parse_sub(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("sub: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::SubReg16Reg16 { dst, src });
    }
    if lhs == "sp" {
        let imm = parse_imm16(rhs)
            .ok_or_else(|| AsmError::new(line_no, format!("sub sp,?: bad imm `{rhs}`")))?;
        return Ok(Instr::SubSpImm(imm as u16));
    }
    // `sub ax,word ptr [si]` — deref through SI as RHS (fixture 201).
    if lhs == "ax" && rhs == "word ptr [si]" {
        return Ok(Instr::SubAxFromSiPtr);
    }
    // `sub ax,word ptr [di]` — deref through DI as RHS, no displacement (the
    // no-disp `mod=00` form `2b 05`, not `SubAxDiDisp{disp:0}` which would emit a
    // redundant `[di+0]`). Mirrors `AddAxFromDiPtr`. Fixture 4241 (`*a - *b` of
    // int pointers held in SI/DI).
    if lhs == "ax" && rhs == "word ptr [di]" {
        return Ok(Instr::SubAxFromDiPtr);
    }
    if lhs == "ax" {
        if let Some(disp) = parse_word_si_disp(rhs) {
            return Ok(Instr::SubAxSiDisp { disp });
        }
        if let Some(disp) = parse_word_di_disp(rhs) {
            return Ok(Instr::SubAxDiDisp { disp });
        }
        // `sub ax,word ptr <group>:<sym>[+N]` — memory-direct subtract
        // from a data-segment global into AX (`2B 06 lo hi`). Mirror of
        // `AddAxGroupSym`. Fixture 4197 (`r - u.i`).
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SubReg16GroupSym {
                reg: Reg16::Ax,
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `sub ax,offset <group>:<sym>[+N]` — subtract the symbol's
        // link-time address as an immediate (`2D lo hi`). Mirror of
        // `AddAxOffsetGroupSym`. Used for pointer-difference against a
        // global array decayed to its base. Fixture 4226 (`best - a`).
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SubAxOffsetGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
    }
    // `sub al,imm8` — AL-specific 2-byte encoding (companion to
    // `AddAlImm8`).
    if lhs == "al" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::SubAlImm8 { imm: imm as u8 });
        }
        if let Some(offset) = parse_byte_bp_relative(rhs) {
            return Ok(Instr::SubAlBpRel { offset });
        }
    }
    // `sub <reg8>, <reg8>` — char compound `-=` between two byte
    // registers (fixture analog of 665 with `-=`).
    if let (Some(dst), Some(src)) = (Reg8::parse(lhs), Reg8::parse(rhs)) {
        return Ok(Instr::SubReg8Reg8 { dst, src });
    }
    // `sub ax, imm16` — AX-accumulator form (2D lo hi, 3 bytes).
    // Same length as the imm8sx form but BCC picks this shape for
    // AX (fixture 3578).
    if lhs == "ax" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::SubAxImm { imm: imm as u16 });
        }
    }
    // `sub <reg16>, imm` — imm8sx form first (3 bytes), then imm16
    // (4 bytes). Mirrors the AddReg16Imm8Sx / AddReg16Imm16 split.
    // Fixture 564 (`p -= 2;` on int-pointer in SI → `sub si, 4`).
    if let Some(reg) = Reg16::parse(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::SubReg16Imm8Sx { reg, imm });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::SubReg16Imm16 { reg, imm });
        }
        // `sub <reg16>, word ptr [bp+N]` — generic register-vs-stack
        // compound `-=` on a non-AX reg local (fixture 660). AX uses
        // its dedicated `SubAxBpRel` variant (parse_alu_ax_mem below).
        if !matches!(reg, Reg16::Ax) {
            if let Some(offset) = parse_bp_relative(rhs) {
                return Ok(Instr::SubReg16BpRel { reg, offset });
            }
        }
        // `sub <reg16>, word ptr <group>:<sym>[+N]` — memory-direct
        // subtract from a global into a non-AX, non-DX register. AX is
        // handled above; DX keeps its dedicated `SubDxGroupSym` (same
        // bytes). Mirror of `AddReg16GroupSym`.
        if !matches!(reg, Reg16::Ax | Reg16::Dx)
            && let Some((group, symbol)) = parse_group_symbol(rhs)
        {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SubReg16GroupSym {
                reg,
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
    }
    // `sub word ptr <group>:<sym>[+N], <reg16>` — fixture 571 sibling.
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(reg) = Reg16::parse(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SubGroupSymReg16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    // `sub byte ptr [si], <reg8>` — char-via-pointer arith `-=`.
    if lhs == "byte ptr [si]" {
        if let Some(src) = Reg8::parse(rhs) {
            return Ok(Instr::SubSiPtrReg8 { src });
        }
    }
    // `sub byte ptr <group>:<sym>[+N], <reg8>` — char compound `-=`
    // on a global (fixture 681).
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        if let Some(reg) = Reg8::parse(rhs) {
            return Ok(Instr::SubGroupSymReg8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    // `sub dx, word ptr <group>:<sym>[+N]` — long-to-long sub
    // low-half (fixture 220).
    if lhs == "dx" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SubDxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `sub dx, word ptr [bp+N]` — low-half stack-local long sub.
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::SubDxBpRel { offset });
        }
    }
    // `sub word ptr <group>:<sym>[+N], imm8sx` — read-modify-write
    // on a data-segment global (low-half of long `g--`, fixture 250).
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SubGroupSymImm8Sx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
    }
    // Bare-symbol `sub word ptr _g[+N], imm8sx` — huge-model
    // `g -= K`. Fixture 3877.
    if let Some(symbol) = lhs.strip_prefix("word ptr ")
        && let Some(first) = symbol.chars().next()
        && (first == '_' || first == '@')
        && !symbol.contains(':')
        && !symbol.contains('[')
        && let Some(imm) = parse_imm8_signed(rhs)
    {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::SubSymImm8Sx {
            symbol: sym.to_string(),
            offset,
            imm,
        });
    }
    // `sub word ptr [bp+N], imm8sx` — long-local compound `-=` low
    // half (fixture analog of 288).
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::SubBpRelImm8 { offset, imm });
        }
        // `sub word ptr [bp+N], dx` — long-stack compound `-=` low
        // half with register-loaded RHS (fixture 340).
        if rhs == "dx" {
            return Ok(Instr::SubBpRelDx { offset });
        }
        // `sub word ptr [bp+N], ax` — long-stack `-= int` low half
        // (sibling of AddBpRelAx).
        if rhs == "ax" {
            return Ok(Instr::SubBpRelAx { offset });
        }
        // `sub word ptr [bp+N], <reg16>` — generalized form.
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::SubBpRelReg16 { reg, offset });
        }
    }
    // `sub byte ptr [bp+N], al` — char compound `-=`.
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if rhs == "al" {
            return Ok(Instr::SubBpRelByteAl { offset });
        }
    }
    // `sub word ptr [si], imm8sx` — long-pointer `*p -= K` low half.
    if lhs == "word ptr [si]" {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::SubSiPtrImm8 { imm });
        }
        if rhs == "ax" {
            return Ok(Instr::SubSiPtrAx);
        }
    }
    // `sub word ptr [bx+disp8], ax` — sibling of `AddBxDispAx`
    // (fixture 862).
    if rhs == "ax" && lhs == "word ptr [bx]" {
        return Ok(Instr::SubBxPtrAx);
    }
    if rhs == "ax"
        && let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::SubBxDispAx { disp });
    }
    // `sub word ptr [si+disp8], ax` — sibling of `AddSiDispAx`
    // (fixture 863).
    if rhs == "ax"
        && let Some(disp) = parse_word_si_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::SubSiDispAx { disp });
    }
    // `sub word ptr [bx+disp8],<imm8sx>` — const-RHS form
    // (sibling of `AddBxDispImm8`).
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm8_signed(rhs)
    {
        return Ok(Instr::SubBxDispImm8 { disp, imm });
    }
    // `sub word ptr <group>:<sym>[bx], imm/reg` — indexed-element
    // compound sub. Mirror of `AddGroupSymBxDispImm*/Reg16`.
    if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::SubGroupSymBxDispImm8Sx {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm,
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::SubGroupSymBxDispImm16 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm: imm as u16,
            });
        }
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::SubGroupSymBxDispReg16 {
                reg,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
    }
    // Otherwise: try the AX/mem form.
    parse_alu_ax_mem(operands, line_no, "sub", |o| Instr::SubAxBpRel { offset: o })
}
/// `sbb ax, word ptr <group>:<sym>[+N]` — subtract-with-borrow,
/// long-arithmetic high-half (fixture 220).
pub(crate) fn parse_sbb(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("sbb: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::SbbReg16Reg16 { dst, src });
    }
    if lhs == "ax" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SbbAxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::SbbAxImm16 { imm: imm as u16 });
        }
        // `sbb ax, word ptr [bp+N]` — high-half borrow for stack-
        // local long sub where result goes to memory (fixture 330).
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::SbbAxBpRel { offset });
        }
    }
    // `sbb <reg16>, imm8sx` — high-half borrow back-propagation in
    // the long unary-neg-at-return idiom (`sbb dx, 0` after `neg
    // dx / neg ax`). Encoded as `83 D(reg) ii` (fixture 371).
    if let Some(reg) = Reg16::parse(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::SbbReg16Imm8Sx { reg, imm });
        }
    }
    // `sbb dx, word ptr [bp+N]` — long return-arith high-half
    // borrow (fixture 285's `return a - b;` analog).
    if lhs == "dx" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::SbbDxBpRel { offset });
        }
    }
    // `sbb word ptr <group>:<sym>[+N], imm8sx` — high-half borrow
    // propagation for long `g--` (fixture 250).
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SbbGroupSymImm8Sx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
        // `sbb word ptr <group>:<sym>[+N], ax` — high-half borrow
        // partner for long-global `g -= h` (fixture 735).
        if rhs == "ax" {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SbbGroupSymAx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `sbb word ptr <group>:<sym>[+N], dx` — long-global
        // `-= int` widening high half.
        if rhs == "dx" {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::SbbGroupSymDx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
    }
    // `sbb word ptr [bp+N], imm8sx` — long-local compound `-=`
    // high half borrow propagation.
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::SbbBpRelImm8 { offset, imm });
        }
        // `sbb word ptr [bp+N], ax` — long-stack compound `-=` high
        // half borrow from register-loaded RHS (fixture 340).
        if rhs == "ax" {
            return Ok(Instr::SbbBpRelAx { offset });
        }
        // `sbb word ptr [bp+N], dx` — sibling for `-= int` cwd path.
        if rhs == "dx" {
            return Ok(Instr::SbbBpRelDx { offset });
        }
    }
    // `sbb word ptr [si+disp], imm8sx` — long-pointer `*p -= K`
    // high-half borrow propagation.
    if let Some(disp) = parse_word_si_disp(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::SbbSiDispImm8 { disp, imm });
        }
        // `sbb word ptr [si+disp], dx` — long `*p -= int x` high
        // half borrow.
        if rhs == "dx" {
            return Ok(Instr::SbbSiDispDx { disp });
        }
    }
    // `sbb word ptr [bx+disp], imm8sx` — long-pointer subscript
    // compound high-half borrow (sibling of `AdcBxDispImm8`).
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm8_signed(rhs)
    {
        return Ok(Instr::SbbBxDispImm8 { disp, imm });
    }
    Err(AsmError::new(
        line_no,
        format!("sbb: unsupported operand form `{operands}`"),
    ))
}
/// `adc ax, imm16` — add-with-carry to AX (fixture 207). Also
/// `adc ax, word ptr <group>:<sym>[+N]` for long-to-long add
/// high-half (fixture 219).
pub(crate) fn parse_adc(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("adc: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::AdcReg16Reg16 { dst, src });
    }
    if lhs == "ax" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AdcAxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AdcAxImm16 { imm: imm as u16 });
        }
    }
    // `adc <reg16>, imm8sx` — high-half carry propagation for the
    // long return-arith path `return g + K` where the high reg is
    // DX (ABI return convention). Encoded as `83 D(reg) ii`
    // (fixture 362).
    if let Some(reg) = Reg16::parse(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AdcReg16Imm8Sx { reg, imm });
        }
    }
    // `adc dx, word ptr <group>:<sym>[+N]` — long-arithmetic
    // high-half carry propagation for the commuted `i + g` shape
    // (fixture 281). Also `adc dx, word ptr [bp+N]` for the long
    // return-arith pattern (fixture 285).
    if lhs == "dx" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AdcDxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::AdcDxBpRel { offset });
        }
    }
    // `adc ax, word ptr [bp+N]` — high-half carry for stack-local
    // long arithmetic where the result goes to memory (AX=high).
    // Fixture 329.
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::AdcAxBpRel { offset });
        }
    }
    // `adc word ptr <group>:<sym>[+N], imm8sx` — high-half carry
    // propagation for long `g++` etc. (fixture 249).
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AdcGroupSymImm8Sx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
        // `adc word ptr <group>:<sym>[+N], ax` — long compound `+=`
        // high half carry against a global / struct-field destination
        // with the register-loaded RHS high half. Fixture 391.
        if rhs == "ax" {
            return Ok(Instr::AdcGroupSymAx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `adc word ptr <group>:<sym>[+N], dx` — long-global
        // `+= int` widening high half (fixture 755, where the
        // cwd-derived sign-extension lives in DX).
        if rhs == "dx" {
            return Ok(Instr::AdcGroupSymDx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
    }
    // `adc word ptr [bp+N], imm8sx` — long-local compound `+=` high
    // half carry propagation (fixture 288).
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AdcBpRelImm8 { offset, imm });
        }
        // `adc word ptr [bp+N], ax` — long-stack compound `+=` high
        // half carry propagation from register-loaded RHS (339).
        if rhs == "ax" {
            return Ok(Instr::AdcBpRelAx { offset });
        }
        // `adc word ptr [bp+N], dx` — long-stack `+= int` high-half
        // carry propagation; DX holds the cwd sign-extension
        // (fixture 765).
        if rhs == "dx" {
            return Ok(Instr::AdcBpRelDx { offset });
        }
    }
    // `adc word ptr [si+disp], imm8sx` — long-pointer `*p += K`
    // high-half carry propagation (fixture 311).
    if let Some(disp) = parse_word_si_disp(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AdcSiDispImm8 { disp, imm });
        }
        // `adc word ptr [si+disp], ax` — long `*p += y` high half
        // carry from register-loaded RHS (fixture 398).
        if rhs == "ax" {
            return Ok(Instr::AdcSiDispAx { disp });
        }
        // `adc word ptr [si+disp], dx` — long `*p += int x` high
        // half: DX holds cwd sign-extension (fixture 849).
        if rhs == "dx" {
            return Ok(Instr::AdcSiDispDx { disp });
        }
    }
    // `adc word ptr [bx+disp], imm8sx` — long-pointer subscript
    // compound high-half carry (fixture 901: `long *p; p[K] += K`).
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm8_signed(rhs)
    {
        return Ok(Instr::AdcBxDispImm8 { disp, imm });
    }
    Err(AsmError::new(
        line_no,
        format!("adc: unsupported operand form `{operands}`"),
    ))
}
/// `add <lhs>,<rhs>` covers `add ax,word ptr [bp+N]` (fixture 113),
/// `add <reg16>,<reg16>` (fixture 127: `add ax,si`), and
/// `add ax,word ptr DGROUP:<sym>` (fixture 131: globals).
pub(crate) fn parse_add(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("add: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::AddReg16Reg16 { dst, src });
    }
    if lhs == "sp" {
        let imm = parse_imm16(rhs)
            .ok_or_else(|| AsmError::new(line_no, format!("add sp,?: bad imm `{rhs}`")))?;
        return Ok(Instr::AddSpImm(imm as u16));
    }
    if lhs == "ax" {
        if rhs == "word ptr [si]" {
            return Ok(Instr::AddAxFromSiPtr);
        }
        if rhs == "word ptr [di]" {
            return Ok(Instr::AddAxFromDiPtr);
        }
        if let Some(disp) = parse_word_si_disp(rhs) {
            return Ok(Instr::AddAxSiDisp { disp });
        }
        if let Some(disp) = parse_word_di_disp(rhs) {
            return Ok(Instr::AddAxDiDisp { disp });
        }
        if let Some(disp) = parse_word_bx_disp(rhs)
            && let Ok(disp8) = i8::try_from(disp)
            && disp8 != 0
        {
            return Ok(Instr::AddAxBxDisp { disp: disp8 });
        }
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::AddAxBpRel { offset });
        }
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AddAxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AddAxOffsetGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // Bare-symbol `add ax, word ptr _g[+N]` — huge-model
        // companion to `MovAxSym`. Fixture 3751.
        if let Some(symbol) = rhs.strip_prefix("word ptr ")
            && let Some(first) = symbol.chars().next()
            && (first == '_' || first == '@')
            && !symbol.contains(':')
            && !symbol.contains('[')
        {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AddAxSym {
                symbol: sym.to_string(),
                offset,
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AddAxImm { imm });
        }
    }
    // Bare-symbol `add word ptr _g[+N], imm8sx` — huge-model
    // `g += K`. Fixture 3874.
    if let Some(symbol) = lhs.strip_prefix("word ptr ")
        && let Some(first) = symbol.chars().next()
        && (first == '_' || first == '@')
        && !symbol.contains(':')
        && !symbol.contains('[')
        && let Some(imm) = parse_imm8_signed(rhs)
    {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::AddSymImm8Sx {
            symbol: sym.to_string(),
            offset,
            imm,
        });
    }
    // `add al,imm8` — AL-specific 2-byte encoding (fixture 529).
    if lhs == "al" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::AddAlImm8 { imm: imm as u8 });
        }
        if let Some(offset) = parse_byte_bp_relative(rhs) {
            return Ok(Instr::AddAlBpRel { offset });
        }
    }
    // `add cl, byte ptr [bp+N]` — byte-direct shift-count
    // accumulation (fixture 3634, `x << (a + b)`).
    if lhs == "cl"
        && let Some(offset) = parse_byte_bp_relative(rhs)
    {
        return Ok(Instr::AddClBpRel { offset });
    }
    // `add <reg8>, <reg8>` — char compound `+=` between two byte
    // registers (fixture 665: `add dl, al` = `02 D0`).
    if let (Some(dst), Some(src)) = (Reg8::parse(lhs), Reg8::parse(rhs)) {
        return Ok(Instr::AddReg8Reg8 { dst, src });
    }
    // `add <reg16>, imm` for non-AX dst (AX uses the shorter
    // `05 lo hi` form via `AddAxImm`). Pick imm8sx (`83 C(rm) ii`,
    // 3 bytes — fixture 207) when the immediate fits; fall back to
    // imm16 (`81 C(rm) lo hi`, 4 bytes — fixture 275) otherwise.
    if let Some(reg) = Reg16::parse(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AddReg16Imm8Sx { reg, imm });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AddReg16Imm16 { reg, imm });
        }
        // `add <reg16>, word ptr <group>:<sym>[bx+disp]` — bx-indexed
        // load + add for `<reg> += <global-arr>[<var>]` (fixture
        // 1462). Tried before the plain group-symbol path so `[bx]`
        // in the symbol part isn't swallowed.
        if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(rhs) {
            return Ok(Instr::AddReg16GroupSymBxDisp {
                reg,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
        // `add <reg16>, word ptr [bx]` — memory-direct add through
        // BX (fixture 1822: `sum += a[i]` for stack int array after
        // address resolves into BX). Also covers `add ax, word ptr
        // [bx]` (fixture 3003); the encoding `03 07` is the same.
        if rhs == "word ptr [bx]" {
            return Ok(Instr::AddReg16FromBxPtr { reg });
        }
        // DI/SI siblings (fixture 1325). DI/SI variants of the AX
        // case have dedicated AddAxFromSiPtr/AddAxFromDiPtr
        // shapes — keep the AX exclusion here so the AX form takes
        // its own encoding above.
        if !matches!(reg, Reg16::Ax) && rhs == "word ptr [di]" {
            return Ok(Instr::AddReg16FromDiPtr { reg });
        }
        if !matches!(reg, Reg16::Ax) && rhs == "word ptr [si]" {
            return Ok(Instr::AddReg16FromSiPtr { reg });
        }
        // `add <reg16>, word ptr <group>:<sym>[+N]` — memory-direct
        // add from a global to a non-AX register. AX uses
        // `AddAxGroupSym`. Fixture 1303 (`a += g`).
        if !matches!(reg, Reg16::Ax)
            && let Some((group, symbol)) = parse_group_symbol(rhs)
        {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AddReg16GroupSym {
                reg,
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `add <reg16>, word ptr [bp+N]` — generic register-vs-stack
        // compound `+=` on a non-AX reg local (fixture 661). AX uses
        // its dedicated `AddAxBpRel` variant above.
        if !matches!(reg, Reg16::Ax) {
            if let Some(offset) = parse_bp_relative(rhs) {
                return Ok(Instr::AddReg16BpRel { reg, offset });
            }
        }
        // `add <reg16>, word ptr [si+N]` / `[di+N]` — generic dst-reg
        // sibling. Fixture 3343 (`s += p->v` with both s and p in
        // registers).
        if !matches!(reg, Reg16::Ax) {
            if let Some(disp) = parse_word_si_disp(rhs) {
                return Ok(Instr::AddReg16SiDisp { reg, disp });
            }
            if let Some(disp) = parse_word_di_disp(rhs) {
                return Ok(Instr::AddReg16DiDisp { reg, disp });
            }
        }
    }
    // `add dx, word ptr <group>:<sym>[+N]` — long-arithmetic low-
    // half add against a memory operand (fixture 219).
    if lhs == "dx" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AddDxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `add dx, word ptr [bp+N]` — low-half stack-local long add
        // (fixture 329).
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::AddDxBpRel { offset });
        }
    }
    // `add word ptr [si],<imm8>` — read-modify-write through SI.
    // Fixture 182: `p->x += 5` where SI holds `p`.
    if lhs == "word ptr [si]" {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AddSiPtrImm8 { imm });
        }
        // Wide-immediate sibling for constants outside [-128, 127].
        // Fixture 1492 (`*p += 1000`).
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AddSiPtrImm16 { imm: imm as u16 });
        }
        // `add word ptr [si], dx` — long `*p += y` low half through
        // a register-resident long pointer in SI (fixture 398).
        if rhs == "dx" {
            return Ok(Instr::AddSiPtrDx);
        }
        // `add word ptr [si], ax` — int `*p += y` through SI
        // (fixture 838).
        if rhs == "ax" {
            return Ok(Instr::AddSiPtrAx);
        }
    }
    // `add word ptr <group>:<sym>[bx], imm/reg` — indexed-element
    // compound add. Fixture 2949 (`arr[i] += K`), 3593 (`arr[i] +=
    // arr[j]`).
    if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AddGroupSymBxDispImm8Sx {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm,
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AddGroupSymBxDispImm16 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm: imm as u16,
            });
        }
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::AddGroupSymBxDispReg16 {
                reg,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
    }
    // `add byte ptr <group>:<sym>[bx], <reg8>` — byte sibling
    // (fixture 3522: char arr += char v).
    if let Some((group, symbol, disp)) = parse_byte_group_symbol_bx_disp(lhs)
        && let Some(reg) = Reg8::parse(rhs)
    {
        return Ok(Instr::AddGroupSymBxDispReg8 {
            reg,
            group: group.to_string(),
            symbol: symbol.to_string(),
            disp,
        });
    }
    // `add byte ptr <group>:<sym>[si], <reg8>` and DI variant.
    if let Some(reg) = Reg8::parse(rhs)
        && let Some((group, symbol, disp)) = parse_byte_group_symbol_reg_disp(lhs, "si")
    {
        return Ok(Instr::AddGroupSymSiDispReg8 {
            reg,
            group: group.to_string(),
            symbol: symbol.to_string(),
            disp,
        });
    }
    if let Some(reg) = Reg8::parse(rhs)
        && let Some((group, symbol, disp)) = parse_byte_group_symbol_reg_disp(lhs, "di")
    {
        return Ok(Instr::AddGroupSymDiDispReg8 {
            reg,
            group: group.to_string(),
            symbol: symbol.to_string(),
            disp,
        });
    }
    // `add word ptr [bx+disp8], ax` — global-pointer subscript
    // compound `int *p; p[K] += y` where BCC loaded the pointer
    // into BX (fixture 862, 879).
    if rhs == "ax" && lhs == "word ptr [bx]" {
        return Ok(Instr::AddBxPtrAx);
    }
    if rhs == "ax"
        && let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::AddBxDispAx { disp });
    }
    // `add word ptr [si+disp8], ax` — stack-local pointer subscript
    // compound where BCC placed the pointer in SI (fixture 863).
    // disp=0 stays with the existing `AddSiPtrAx` 2-byte form.
    if rhs == "ax"
        && let Some(disp) = parse_word_si_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::AddSiDispAx { disp });
    }
    // `add word ptr [bx],<imm8>` — same shape via BX. Fixture 197
    // (`*p += 5` for a global pointer that's been loaded into BX).
    if lhs == "word ptr [bx]" {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AddBxPtrImm8 { imm });
        }
    }
    // `add word ptr [bx+disp8],<imm8sx>` — const-RHS form of the
    // global-pointer subscript compound (fixture 864).
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm8_signed(rhs)
    {
        return Ok(Instr::AddBxDispImm8 { disp, imm });
    }
    // `add word ptr [bp+N],<imm8>` — read-modify-write on a stack
    // local. Fixture 184: `a[1] += 5` folds to bp-relative add.
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AddBpRelImm8 { offset, imm });
        }
        // `add word ptr [bp+N], dx` — long-stack compound `+=` low
        // half (fixture 339).
        if rhs == "dx" {
            return Ok(Instr::AddBpRelDx { offset });
        }
        // `add word ptr [bp+N], ax` — long-stack `+= int` low half
        // (fixture 765, AX holds int RHS).
        if rhs == "ax" {
            return Ok(Instr::AddBpRelAx { offset });
        }
        // `add word ptr [bp+N], <reg16>` — generalized form. Fixture
        // 1980 (`e += a` with e stack, a in SI).
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::AddBpRelReg16 { reg, offset });
        }
    }
    // `add byte ptr [bp+N], al` — char compound `+=` with char-lvalue
    // RHS (sibling of XorBpRelByteAl).
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if rhs == "al" {
            return Ok(Instr::AddBpRelByteAl { offset });
        }
    }
    // `add word ptr <group>:<sym>[+N], imm` — read-modify-write
    // on a data-segment global. Prefer imm8sx (`83 06 ... ii`,
    // 5 bytes — fixture 249's `g++`) when the immediate fits;
    // fall back to imm16 (`81 06 ... lo hi`, 6 bytes — fixture
    // 276's `g += 1000`) otherwise.
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AddGroupSymImm8Sx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AddGroupSymImm16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
        // `add word ptr <group>:<sym>[+N], dx` — long compound `+=`
        // low half against a global / struct-field destination with
        // variable RHS already in DX. Fixture 391.
        if rhs == "dx" {
            return Ok(Instr::AddGroupSymDx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `add word ptr <group>:<sym>[+N], <reg16>` — generic
        // memory-dest, reg-source. Fixture 571 (`a += b;` between
        // two int globals → `add [_a], ax`).
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::AddGroupSymReg16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    // `add byte ptr [si], <reg8>` — char-via-pointer arith with
    // variable RHS already in the byte register (fixture 713:
    // `add byte ptr [si], al`).
    if lhs == "byte ptr [si]" {
        if let Some(src) = Reg8::parse(rhs) {
            return Ok(Instr::AddSiPtrReg8 { src });
        }
    }
    // `add byte ptr <group>:<sym>[+N], <reg8>` — char compound `+=`
    // on a data-segment global (fixture 680).
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        if let Some(reg) = Reg8::parse(rhs) {
            return Ok(Instr::AddGroupSymReg8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    Err(AsmError::new(
        line_no,
        format!("add: unsupported operand form `{operands}`"),
    ))
}
/// `cmp <lhs>,<rhs>` covers three forms: `cmp ax,word ptr [bp+N]`
/// (16-bit memory), `cmp <reg8>,<imm8>` (fixture 124), and
/// `cmp <reg16>,<imm8>` sign-extended (fixture 126: `cmp si,10`).
pub(crate) fn parse_cmp(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("cmp: expected `lhs,rhs`, got {operands:?}"))
    })?;
    // `cmp al, byte ptr [bp+N]` — char-vs-char compare peephole.
    // Fixture 951.
    if lhs == "al" {
        if let Some(offset) = parse_byte_bp_relative(rhs) {
            return Ok(Instr::CmpAlBpRel { offset });
        }
        // Char-ptr-deref compares (fixture 1352).
        if rhs == "byte ptr [si]" {
            return Ok(Instr::CmpAlFromSiPtr);
        }
        if rhs == "byte ptr [di]" {
            return Ok(Instr::CmpAlFromDiPtr);
        }
        if rhs == "byte ptr [bx]" {
            return Ok(Instr::CmpAlFromBxPtr);
        }
        // `cmp al, byte ptr [bp+si+disp]` — char-array element
        // compared against another such element while the index
        // is in SI. Fixture 2488 (`a[i] != b[i]`).
        if let Some(disp) = parse_byte_bp_si_disp(rhs) {
            return Ok(Instr::CmpAlBpSiDisp { disp });
        }
    }
    // `cmp byte ptr [bp+si+disp], imm8` — char-array element
    // compared against a constant while the index is in SI. Used
    // by `a[i] != 0` for-loop terminators. Fixture 2488.
    if let Some(disp) = parse_byte_bp_si_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::CmpBpSiDispImm8 { disp, imm });
    }
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::CmpAxBpRel { offset });
        }
        // `cmp ax, word ptr [si|di|bx]` — register-pointer deref
        // sources. Fixtures 1352, 2203, 2362, 3418 (`*p` cmp via DI).
        if rhs == "word ptr [di]" {
            return Ok(Instr::CmpAxFromDiPtr);
        }
        if rhs == "word ptr [si]" {
            return Ok(Instr::CmpAxFromSiPtr);
        }
        if rhs == "word ptr [bx]" {
            return Ok(Instr::CmpAxFromBxPtr);
        }
        // `cmp ax,K` — BCC uses the special AX-imm16 opcode (3D) for
        // every constant K, not the generic 83 F8 form.
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::CmpAxImm { imm });
        }
        // `cmp ax, word ptr <group>:<sym>[+N]` — high-half compare
        // for the signed long-compare 3-jump pattern (fixture 234).
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::CmpAxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
    }
    // `cmp <reg16>, word ptr [bp+N]` — generic register-vs-stack
    // compare. Fixture 648 uses this for `cmp si, word ptr [bp-2]`.
    // AX/DX have their dedicated variants above; this catches the
    // remaining registers.
    if let Some(reg) = Reg16::parse(lhs) {
        if !matches!(reg, Reg16::Ax | Reg16::Dx) {
            if let Some(offset) = parse_bp_relative(rhs) {
                return Ok(Instr::CmpReg16BpRel { reg, offset });
            }
        }
        // `cmp <reg16>,offset <group>:<sym>[+N]` — compare a pointer
        // register against a global array element's link-time address
        // (`81 /7 modrm lo hi`). Fixture 4226 (`cmp si,offset
        // DGROUP:_a+12` for the loop guard `p < a + 6`).
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::CmpReg16OffsetGroupSym {
                reg,
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
    }
    // `cmp word ptr [bp+N], <reg16>` — memory-on-left compare.
    // Fixture 3588 (`a > b` with a stack, b in SI → `cmp word ptr
    // [bp+4], si`). Tried before bp-relative immediate forms below
    // so the `, <reg>` rhs catches.
    if let Some(offset) = parse_bp_relative(lhs)
        && let Some(reg) = Reg16::parse(rhs)
    {
        return Ok(Instr::CmpBpRelReg16 { reg, offset });
    }
    // `cmp dx, word ptr <group>:<sym>[+N]` — low-half companion for
    // the signed long-compare 3-jump pattern (fixture 234).
    if lhs == "dx" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::CmpDxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `cmp dx, word ptr [bp+N]` — long-vs-long 3-jump compare on
        // stack locals (fixture 297).
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::CmpDxBpRel { offset });
        }
    }
    // `cmp word ptr [bp+N],imm8` — compare stack local to small imm.
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::CmpBpRelImm8 { offset, imm });
        }
        // Wide-immediate sibling (`81 7E dd lo hi`) for constants
        // that don't fit imm8sx. Fixture 563.
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::CmpBpRelImm16 { offset, imm: imm as u16 });
        }
    }
    // `cmp word ptr [bx+disp8], imm8sx` — pointer-subscript zero-
    // test (fixture 889: `if (p[K])` → `cmp word ptr [bx+K*2], 0`).
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm8_signed(rhs)
    {
        return Ok(Instr::CmpBxDispImm8 { disp, imm });
    }
    // `cmp word ptr <group>:<sym>[bx], imm` — indexed-array zero-test
    // (fixture 1309: `while (a[i])` for int global array). Try
    // imm8sx form first (1 byte saved), fall back to imm16.
    if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::CmpGroupSymBxDispImm8 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm,
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::CmpGroupSymBxDispImm16 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm: imm as u16,
            });
        }
    }
    // `cmp byte ptr <group>:<sym>[bx], imm8` — byte-form sibling
    // for char-array boolean tests.
    if let Some((group, symbol, disp)) = parse_byte_group_symbol_bx_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::CmpByteGroupSymBxDispImm8 {
            group: group.to_string(),
            symbol: symbol.to_string(),
            disp,
            imm: imm as u8,
        });
    }
    // `cmp byte ptr <group>:<sym>[+N], imm8` — char-global compare
    // (`80 3E lo hi ii`, 5 bytes). Used by `if (c == 'A')` for
    // char globals (fixture 452).
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::CmpByteGroupSymImm8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm: imm as u8,
            });
        }
    }
    // `cmp byte ptr [bp+N], imm8` — char-local compare
    // (`80 7E dd ii`, 4 bytes). Fixture 524.
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::CmpByteBpRelImm8 { offset, imm: imm as u8 });
        }
    }
    // `cmp byte ptr [si], imm8` — `80 3C ii` (fixture 636's `while
    // (*p)` with `p` enregistered in SI).
    if lhs == "byte ptr [si]" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::CmpByteSiPtrImm8 { imm: imm as u8 });
        }
    }
    // `cmp word ptr [si|di|bx], imm` — word-form sibling. Prefer
    // imm8sx (saves 1 byte), fall back to imm16.
    if lhs == "word ptr [si]" {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::CmpWordSiPtrImm8Sx { imm });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::CmpWordSiPtrImm16 { imm: imm as u16 });
        }
    }
    if lhs == "word ptr [di]" {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::CmpWordDiPtrImm8Sx { imm });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::CmpWordDiPtrImm16 { imm: imm as u16 });
        }
    }
    if lhs == "word ptr [bx]" {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::CmpWordBxPtrImm8Sx { imm });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::CmpWordBxPtrImm16 { imm: imm as u16 });
        }
    }
    // `cmp byte ptr [bx], imm8` — `80 3F ii` (fixture 2027's
    // `while (*s++)` with the pre-update pointer parked in BX).
    if lhs == "byte ptr [bx]" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::CmpByteBxPtrImm8 { imm: imm as u8 });
        }
    }
    // `cmp byte ptr [di], imm8` — `80 3D ii` (fixture 1311's
    // `while (*++p)` with char* p enregistered in DI).
    if lhs == "byte ptr [di]" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::CmpByteDiPtrImm8 { imm: imm as u8 });
        }
    }
    // `cmp word ptr [si+disp], imm8sx` — Grp1 r/m16,imm8sx with
    // SI-indirect addressing. Used by the arrow-field memory-direct
    // compare peephole (`p->x == K` with p in SI). Fixture 1007.
    if let Some(disp) = parse_word_si_disp(lhs)
        && let Some(imm) = parse_imm8_signed(rhs)
    {
        return Ok(Instr::CmpWordSiDispImm8Sx { disp: i16::from(disp), imm });
    }
    // `cmp word ptr <group>:<sym>[+N], imm` — long const-compare
    // chained-cmp pattern. Prefer imm8sx (`83 3E ...`, 5 bytes —
    // fixture 223) when it fits; fall back to imm16 (`81 3E ...`,
    // 6 bytes — fixture 282) for wider constants.
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::CmpGroupSymImm8Sx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::CmpGroupSymImm16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
    }
    if let Some(reg) = Reg16::parse(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::CmpReg16Imm8 { reg, imm });
        }
        if let Some(rhs_reg) = Reg16::parse(rhs) {
            return Ok(Instr::CmpReg16Reg16 { lhs: reg, rhs: rhs_reg });
        }
        // Wider-immediate sibling for K not in imm8sx range.
        // AX gets `CmpAxImm` (3-byte 3D form) above; non-AX uses 81.
        if !matches!(reg, Reg16::Ax)
            && let Some(imm) = parse_imm16(rhs)
        {
            return Ok(Instr::CmpReg16Imm16 { reg, imm: imm as u16 });
        }
    }
    if let Some(reg) = Reg8::parse(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            // `cmp al, imm8` has a dedicated 2-byte AL-accumulator
            // form (3C ii), distinct from the 3-byte generic Grp1
            // (80 F8 ii). Fixture 4054 (`if (_AL == 0x80)`).
            if matches!(reg, Reg8::Al) {
                return Ok(Instr::CmpAlImm8 { imm });
            }
            return Ok(Instr::CmpReg8Imm8 { reg, imm });
        }
    }
    // `cmp word ptr <group>:<sym>, <reg16>` — used by the `-N`
    // stack-overflow check (fixture 2129). Match `word ptr` LHS +
    // bare register RHS.
    if let Some((group, symbol)) = parse_group_symbol_with_width(lhs, "word ptr ")
        && let Some(reg) = Reg16::parse(rhs)
    {
        let (sym, _off) = split_sym_offset(symbol);
        return Ok(Instr::CmpGroupSymReg16 {
            group: group.to_string(),
            symbol: sym.to_string(),
            reg,
        });
    }
    Err(AsmError::new(
        line_no,
        format!("cmp: unsupported operand form `{operands}`"),
    ))
}
