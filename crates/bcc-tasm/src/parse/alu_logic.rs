use super::*;

/// `and` covers `and ax,word ptr [bp+N]` (existing) plus the
/// long-arithmetic group-sym forms `and {ax|dx},word ptr <group>:<sym>[+N]`
/// (fixture 221).
pub(crate) fn parse_and(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("and: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::AndReg16Reg16 { dst, src });
    }
    if lhs == "ax" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AndAxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `and ax, imm16` — AX-specific accumulator form (fixture
        // 609's `c & 4` after cbw widening: `25 04 00`).
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AndAxImm16 { imm: imm as u16 });
        }
    }
    // `and al,imm8` — AL-specific 2-byte encoding.
    if lhs == "al" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::AndAlImm8 { imm: imm as u8 });
        }
        if let Some(offset) = parse_byte_bp_relative(rhs) {
            return Ok(Instr::AndAlBpRel { offset });
        }
    }
    // `and <reg8>,imm8` for non-AL byte registers (3-byte generic
    // form). Fixture 556 (`and dl, 0x1F`).
    if let Some(reg) = Reg8::parse(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::AndReg8Imm8 { reg, imm: imm as u8 });
        }
    }
    // `and <reg8>, <reg8>` — char compound `&=` between two byte
    // registers (fixture analog of 665 with `&=`).
    if let (Some(dst), Some(src)) = (Reg8::parse(lhs), Reg8::parse(rhs)) {
        return Ok(Instr::AndReg8Reg8 { dst, src });
    }
    if lhs == "dx" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AndDxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `and dx, word ptr [bp+N]` — low-half stack-local long AND
        // (fixture 333).
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::AndDxBpRel { offset });
        }
    }
    // `and word ptr <group>:<sym>[+N], imm16` — long compound
    // `g &= K` (fixture 253). BCC always picks the imm16 form
    // (`81 26`) for bitwise compound assigns, even when K would
    // fit in i8sx — distinct from arithmetic `+=` which uses 83.
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AndGroupSymImm16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
    }
    // `and word ptr [bp+N], imm16` — long-local compound `&=`
    // (fixture 289). Same imm16 rule as the global path.
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AndBpRelImm16 { offset, imm: imm as u16 });
        }
        if rhs == "dx" {
            return Ok(Instr::AndBpRelDx { offset });
        }
        if rhs == "ax" {
            return Ok(Instr::AndBpRelAx { offset });
        }
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::AndBpRelReg16 { reg, offset });
        }
    }
    // `and byte ptr [bp+N], al` — char compound `&=`.
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if rhs == "al" {
            return Ok(Instr::AndBpRelByteAl { offset });
        }
    }
    // `and <reg16>, word ptr [bp+N]` — generic register-vs-stack
    // bitwise AND for compound `&=` on a non-AX reg local (fixture
    // 655: `x &= y` with x in SI). AX uses the AL-/short form via
    // `parse_alu_ax_mem` below.
    if let Some(reg) = Reg16::parse(lhs) {
        if !matches!(reg, Reg16::Ax) {
            if let Some(offset) = parse_bp_relative(rhs) {
                return Ok(Instr::AndReg16BpRel { reg, offset });
            }
        }
    }
    // `and byte ptr [si], imm8` — char-via-pointer bitwise compound
    // (fixture 712: `*p &= 15`).
    if lhs == "byte ptr [si]" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::AndSiPtrByteImm8 { imm: imm as u8 });
        }
    }
    // `and byte ptr [si+disp]` / `[di+disp], imm8` — char field at an offset.
    if let Some(disp) = parse_byte_si_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::AluByteRegDispImm8 { op: 4, di: false, disp: i16::from(disp), imm: imm as u8 });
    }
    if let Some(disp) = parse_byte_di_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::AluByteRegDispImm8 { op: 4, di: true, disp: i16::from(disp), imm: imm as u8 });
    }
    // `and word ptr [si], imm16` — int `*p &= K` through SI. BCC uses the
    // imm16 form even for small bitwise constants (fixture 4289).
    if lhs == "word ptr [si]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AndSiPtrImm16 { imm: imm as u16 });
        }
        // `and word ptr [si], ax` — int `*p &= y` through SI.
        if rhs == "ax" {
            return Ok(Instr::AndSiPtrAx);
        }
    }
    // `and word ptr [di], imm16` — the DI-pointer sibling.
    if lhs == "word ptr [di]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AndDiPtrImm16 { imm: imm as u16 });
        }
    }
    // `and word ptr [bx], imm16` — the BX-pointer sibling (global ptr).
    if lhs == "word ptr [bx]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AndBxPtrImm16 { imm: imm as u16 });
        }
    }
    // `and word ptr [si+disp], imm16` — struct field compound.
    if let Some(disp) = parse_word_si_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm16(rhs)
    {
        return Ok(Instr::AndSiDispImm16 { disp, imm: imm as u16 });
    }
    // `and word ptr [bx+disp8], ax` — sibling of `AddBxDispAx`
    // (fixture 862).
    if rhs == "ax" && lhs == "word ptr [bx]" {
        return Ok(Instr::AndBxPtrAx);
    }
    if rhs == "ax"
        && let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::AndBxDispAx { disp });
    }
    // `and word ptr [si+disp8], ax` — sibling of `AddSiDispAx`
    // (fixture 863).
    if rhs == "ax"
        && let Some(disp) = parse_word_si_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::AndSiDispAx { disp });
    }
    // `and byte ptr [bx+disp8], al` — char-pointer bitwise compound
    // mem-direct form (fixture 870: `char *p; p[K] &= y`).
    if rhs == "al"
        && let Some(disp) = parse_byte_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::AndBxDispAl { disp });
    }
    // `and word ptr [bx+disp8], <imm>` — const-RHS bitwise compound
    // via global int-pointer subscript (fixture 875: `int *p; p[K]
    // &= 15`). BCC uses imm16 even when value fits i8 — same
    // asymmetry as the flat `g &= K` path.
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm16(rhs)
    {
        return Ok(Instr::AndBxDispImm16 { disp, imm: imm as u16 });
    }
    // `and byte ptr [bp+N], imm8` — char-local-array bitwise
    // compound (fixture 720).
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::AndBpRelByteImm8 { offset, imm: imm as u8 });
        }
    }
    // `and word ptr <group>:<sym>[+N], <reg16>` — long-global
    // `g &= h` (fixture 736).
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(reg) = Reg16::parse(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::AndGroupSymReg16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    // `and byte ptr <group>:<sym>[+N], <reg8>` — char compound `&=`
    // on a global (fixture 682).
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        if let Some(reg) = Reg8::parse(rhs) {
            return Ok(Instr::AndGroupSymReg8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
        // `and byte ptr <group>:<sym>, imm8` — char-global compound
        // `&=` with constant RHS (fixture 685).
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::AndGroupSymImm8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm: imm as u8,
            });
        }
    }
    // `and <reg16>, imm16` — Grp1 /4=AND sibling of OrReg16Imm16.
    if let (Some(reg), Some(imm)) = (Reg16::parse(lhs), parse_imm16(rhs)) {
        return Ok(Instr::AndReg16Imm16 { reg, imm });
    }
    parse_alu_ax_mem(operands, line_no, "and", |o| Instr::AndAxBpRel { offset: o })
}
/// `or <lhs>,<rhs>` covers `or <reg16>,<reg16>` (fixture 132's
/// `or ax,ax` zero-test idiom) and `or ax,word ptr [bp+N]`.
pub(crate) fn parse_or(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("or: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::OrReg16Reg16 { dst, src });
    }
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::OrAxBpRel { offset });
        }
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::OrAxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `or ax, imm16` — AX-specific accumulator form (fixture
        // 611's `x | 8` → `0D 08 00`).
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::OrAxImm16 { imm: imm as u16 });
        }
    }
    // `or al,imm8` — AL-specific 2-byte encoding.
    if lhs == "al" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::OrAlImm8 { imm: imm as u8 });
        }
        if let Some(offset) = parse_byte_bp_relative(rhs) {
            return Ok(Instr::OrAlBpRel { offset });
        }
    }
    if let Some(reg) = Reg8::parse(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::OrReg8Imm8 { reg, imm: imm as u8 });
        }
    }
    // `or <reg8>, <reg8>` — char compound `|=` between two byte
    // registers (fixture 668: `or dl, al` = `0A D0`).
    if let (Some(dst), Some(src)) = (Reg8::parse(lhs), Reg8::parse(rhs)) {
        return Ok(Instr::OrReg8Reg8 { dst, src });
    }
    // `or dx, word ptr <group>:<sym>[+N]` — long bitwise OR low half
    // (fixture 222).
    if lhs == "dx" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::OrDxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `or dx, word ptr [bp+N]` — low-half stack-local long OR
        // (fixture 334).
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::OrDxBpRel { offset });
        }
    }
    // `or <reg16>, word ptr <group>:<sym>[+N]` — memory-direct OR
    // from a global to a non-AX register. Fixture 1383 (`a |= s.x`
    // with a in SI). Tried BEFORE the imm16 form so the parse of
    // `word ptr DGROUP:...` doesn't get mis-handled.
    if let Some(reg) = Reg16::parse(lhs)
        && !matches!(reg, Reg16::Ax | Reg16::Dx)
        && let Some((group, symbol)) = parse_group_symbol(rhs)
    {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::OrReg16GroupSym {
            reg,
            group: group.to_string(),
            symbol: sym.to_string(),
            offset,
        });
    }
    // `or <reg16>, imm16` — Grp1 /1=OR. Used by the long-return
    // bitwise-or-imm path (fixture 2876: `or dx, 0` as the high-half
    // OR with 0x100L → hi=0).
    if let (Some(reg), Some(imm)) = (Reg16::parse(lhs), parse_imm16(rhs)) {
        return Ok(Instr::OrReg16Imm16 { reg, imm });
    }
    // `or word ptr <group>:<sym>[+N], imm16` — long compound
    // `g |= K`. Same imm16-always rule as the `and` companion.
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::OrGroupSymImm16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
    }
    // `or word ptr [bp+N], imm16` — long-local compound `|=`.
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::OrBpRelImm16 { offset, imm: imm as u16 });
        }
        if rhs == "dx" {
            return Ok(Instr::OrBpRelDx { offset });
        }
        if rhs == "ax" {
            return Ok(Instr::OrBpRelAx { offset });
        }
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::OrBpRelReg16 { reg, offset });
        }
    }
    // `or byte ptr [bp+N], al` — char compound `|=`.
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if rhs == "al" {
            return Ok(Instr::OrBpRelByteAl { offset });
        }
    }
    // `or <reg16>, word ptr [bp+N]` — generic register-vs-stack
    // bitwise OR for compound `|=` on a non-AX reg local
    // (fixture 656). AX uses the bp-rel variant via parse_alu_ax
    // earlier in the function (handled by the lhs == "ax" arm).
    if let Some(reg) = Reg16::parse(lhs) {
        if !matches!(reg, Reg16::Ax) {
            if let Some(offset) = parse_bp_relative(rhs) {
                return Ok(Instr::OrReg16BpRel { reg, offset });
            }
        }
    }
    // `or byte ptr [si], imm8` — char-via-pointer `|=`.
    if lhs == "byte ptr [si]" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::OrSiPtrByteImm8 { imm: imm as u8 });
        }
    }
    // `or byte ptr [si+disp]` / `[di+disp], imm8` — char field at an offset.
    if let Some(disp) = parse_byte_si_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::AluByteRegDispImm8 { op: 1, di: false, disp: i16::from(disp), imm: imm as u8 });
    }
    if let Some(disp) = parse_byte_di_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::AluByteRegDispImm8 { op: 1, di: true, disp: i16::from(disp), imm: imm as u8 });
    }
    // `or word ptr [si], K|ax` — int `*p |= …` through SI (imm16 even for
    // small constants, fixture 4288).
    if lhs == "word ptr [si]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::OrSiPtrImm16 { imm: imm as u16 });
        }
        if rhs == "ax" {
            return Ok(Instr::OrSiPtrAx);
        }
    }
    // `or word ptr [di], imm16` — the DI-pointer sibling.
    if lhs == "word ptr [di]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::OrDiPtrImm16 { imm: imm as u16 });
        }
    }
    // `or word ptr [bx], imm16` — the BX-pointer sibling (global ptr).
    if lhs == "word ptr [bx]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::OrBxPtrImm16 { imm: imm as u16 });
        }
    }
    // `or word ptr [si+disp], imm16` — struct field compound.
    if let Some(disp) = parse_word_si_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm16(rhs)
    {
        return Ok(Instr::OrSiDispImm16 { disp, imm: imm as u16 });
    }
    // `or word ptr [bx+disp8], ax` — sibling of `AddBxDispAx`
    // (fixture 862).
    if rhs == "ax" && lhs == "word ptr [bx]" {
        return Ok(Instr::OrBxPtrAx);
    }
    if rhs == "ax"
        && let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::OrBxDispAx { disp });
    }
    // `or word ptr [si+disp8], ax` — sibling of `AddSiDispAx`
    // (fixture 863).
    if rhs == "ax"
        && let Some(disp) = parse_word_si_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::OrSiDispAx { disp });
    }
    // `or byte ptr [bx+disp8], al` — char-pointer bitwise compound
    // (fixture 871: `char *p; p[K] |= y`).
    if rhs == "al"
        && let Some(disp) = parse_byte_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::OrBxDispAl { disp });
    }
    // `or word ptr [bx+disp8], <imm>` — sibling of `AndBxDispImm16`.
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm16(rhs)
    {
        return Ok(Instr::OrBxDispImm16 { disp, imm: imm as u16 });
    }
    // `or byte ptr [bp+N], imm8` — char-local-array `|=`.
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::OrBpRelByteImm8 { offset, imm: imm as u8 });
        }
    }
    // `or word ptr <group>:<sym>[+N], <reg16>` — long-global `|=`.
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(reg) = Reg16::parse(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::OrGroupSymReg16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    // `or byte ptr <group>:<sym>[+N], <reg8>` — char compound `|=`
    // on a global.
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        if let Some(reg) = Reg8::parse(rhs) {
            return Ok(Instr::OrGroupSymReg8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::OrGroupSymImm8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm: imm as u8,
            });
        }
    }
    Err(AsmError::new(
        line_no,
        format!("or: unsupported operand form `{operands}`"),
    ))
}
/// `xor <lhs>,<rhs>` covers two forms: `xor <reg16>,<reg16>` (the
/// canonical zero-the-register idiom) and `xor ax,word ptr [bp+N]`.
pub(crate) fn parse_xor(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("xor: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::XorReg16Reg16 { dst, src });
    }
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::XorAxBpRel { offset });
        }
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::XorAxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `xor ax, imm16` — AX-specific accumulator form (fixture
        // 612's `x ^ 3` → `35 03 00`).
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::XorAxImm16 { imm: imm as u16 });
        }
    }
    // `xor al,imm8` — AL-specific 2-byte encoding.
    if lhs == "al" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::XorAlImm8 { imm: imm as u8 });
        }
        if let Some(offset) = parse_byte_bp_relative(rhs) {
            return Ok(Instr::XorAlBpRel { offset });
        }
    }
    if let Some(reg) = Reg8::parse(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::XorReg8Imm8 { reg, imm: imm as u8 });
        }
    }
    // `xor <reg8>, <reg8>` — char compound `^=` between two byte
    // registers (fixture 669: `xor dl, al` = `32 D0`).
    if let (Some(dst), Some(src)) = (Reg8::parse(lhs), Reg8::parse(rhs)) {
        return Ok(Instr::XorReg8Reg8 { dst, src });
    }
    // `xor dx, word ptr <group>:<sym>[+N]` — long bitwise XOR low
    // half (fixture 224).
    if lhs == "dx" {
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::XorDxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `xor dx, word ptr [bp+N]` — low-half stack-local long XOR.
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::XorDxBpRel { offset });
        }
    }
    // `xor word ptr [di], <reg16>` — memory-direct xor into [di].
    // Fixture 3638 (xor-swap idiom — `*p ^= *q` with q in DI).
    if lhs == "word ptr [di]"
        && let Some(reg) = Reg16::parse(rhs)
    {
        return Ok(Instr::XorDiPtrReg16 { reg });
    }
    // `xor word ptr <group>:<sym>[+N], imm16` — long compound
    // `g ^= K`. Same imm16-always rule as the `and` companion.
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::XorGroupSymImm16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm,
            });
        }
    }
    // `xor word ptr [bp+N], imm16` — long-local compound `^=`.
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::XorBpRelImm16 { offset, imm: imm as u16 });
        }
        if rhs == "dx" {
            return Ok(Instr::XorBpRelDx { offset });
        }
        if rhs == "ax" {
            return Ok(Instr::XorBpRelAx { offset });
        }
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::XorBpRelReg16 { reg, offset });
        }
    }
    // `xor byte ptr [bp+N], al` — char compound `^=` with a
    // char-typed lvalue RHS (fixture 1447).
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if rhs == "al" {
            return Ok(Instr::XorBpRelByteAl { offset });
        }
    }
    // `xor <reg16>, word ptr [bp+N]` — generic register-vs-stack
    // bitwise XOR for compound `^=` on a non-AX reg local
    // (fixture 657).
    if let Some(reg) = Reg16::parse(lhs) {
        if !matches!(reg, Reg16::Ax) {
            if let Some(offset) = parse_bp_relative(rhs) {
                return Ok(Instr::XorReg16BpRel { reg, offset });
            }
        }
    }
    // `xor byte ptr [si], imm8` — char-via-pointer `^=`.
    if lhs == "byte ptr [si]" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::XorSiPtrByteImm8 { imm: imm as u8 });
        }
    }
    // `xor byte ptr [si+disp]` / `[di+disp], imm8` — char field at an offset.
    if let Some(disp) = parse_byte_si_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::AluByteRegDispImm8 { op: 6, di: false, disp: i16::from(disp), imm: imm as u8 });
    }
    if let Some(disp) = parse_byte_di_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::AluByteRegDispImm8 { op: 6, di: true, disp: i16::from(disp), imm: imm as u8 });
    }
    // `xor word ptr [si], K|ax` — int `*p ^= …` through SI (imm16 even for
    // small constants, fixture 4290).
    if lhs == "word ptr [si]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::XorSiPtrImm16 { imm: imm as u16 });
        }
        if rhs == "ax" {
            return Ok(Instr::XorSiPtrAx);
        }
    }
    // `xor word ptr [di], imm16` — the DI-pointer sibling.
    if lhs == "word ptr [di]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::XorDiPtrImm16 { imm: imm as u16 });
        }
    }
    // `xor word ptr [bx], imm16` — the BX-pointer sibling (global ptr).
    if lhs == "word ptr [bx]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::XorBxPtrImm16 { imm: imm as u16 });
        }
    }
    // `xor word ptr [si+disp], imm16` — struct field compound.
    if let Some(disp) = parse_word_si_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm16(rhs)
    {
        return Ok(Instr::XorSiDispImm16 { disp, imm: imm as u16 });
    }
    // `xor word ptr [bx+disp8], ax` — sibling of `AddBxDispAx`
    // (fixture 862).
    if rhs == "ax" && lhs == "word ptr [bx]" {
        return Ok(Instr::XorBxPtrAx);
    }
    if rhs == "ax"
        && let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::XorBxDispAx { disp });
    }
    // `xor word ptr [si+disp8], ax` — sibling of `AddSiDispAx`
    // (fixture 863).
    if rhs == "ax"
        && let Some(disp) = parse_word_si_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::XorSiDispAx { disp });
    }
    // `xor byte ptr [bx+disp8], al` — char-pointer bitwise compound
    // (sibling of `AndBxDispAl` / `OrBxDispAl`).
    if rhs == "al"
        && let Some(disp) = parse_byte_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::XorBxDispAl { disp });
    }
    // `xor word ptr [bx+disp8], <imm>` — sibling of `AndBxDispImm16`.
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm16(rhs)
    {
        return Ok(Instr::XorBxDispImm16 { disp, imm: imm as u16 });
    }
    // `xor byte ptr [bp+N], imm8` — char-local-array `^=`.
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::XorBpRelByteImm8 { offset, imm: imm as u8 });
        }
    }
    // `xor word ptr <group>:<sym>[+N], <reg16>` — long-global `^=`.
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(reg) = Reg16::parse(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::XorGroupSymReg16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    // `xor byte ptr <group>:<sym>[+N], <reg8>` — char compound `^=`
    // on a global.
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        let (sym, offset) = split_sym_offset(symbol);
        if let Some(reg) = Reg8::parse(rhs) {
            return Ok(Instr::XorGroupSymReg8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::XorGroupSymImm8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm: imm as u8,
            });
        }
    }
    // `xor <reg16>, imm16` — Grp1 /6=XOR sibling.
    if let (Some(reg), Some(imm)) = (Reg16::parse(lhs), parse_imm16(rhs)) {
        return Ok(Instr::XorReg16Imm16 { reg, imm });
    }
    Err(AsmError::new(
        line_no,
        format!("xor: unsupported operand form `{operands}`"),
    ))
}
