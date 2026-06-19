use super::*;

pub(crate) fn parse_mov(operands: &str, line_no: usize) -> AsmResult<Instr> {
    // Forms we recognize:
    //   bp,sp                                  → MovBpSp
    //   sp,bp                                  → MovSpBp
    //   ax,<decimal>                           → MovAxImm
    //   ax,word ptr [bp<sign><offset>]         → MovAxBpRel
    //   word ptr [bp<sign><offset>],<decimal>  → MovBpRelImm
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("mov: expected `lhs,rhs`, got {operands:?}"))
    })?;
    // Segment-override prefix on either operand (e.g.
    // `mov word ptr ss:[si], 1234` or `mov ax, word ptr cs:[si]`).
    // Strip the `<seg>:` qualifier, parse the rest as if there were
    // no override, then wrap in `SegOverride`. The existing
    // hardcoded `es:[bx]` shapes used by the far-pointer codegen
    // stay on their own paths and don't hit this — we only fire
    // when stripping the prefix produces a parse for the underlying
    // shape. Fixtures 4063–4068.
    if let Some((seg, stripped)) = strip_segment_override(lhs) {
        let new_operands = format!("{stripped},{rhs}");
        let inner = parse_mov(&new_operands, line_no)?;
        return Ok(Instr::SegOverride { seg, inner: Box::new(inner) });
    }
    if let Some((seg, stripped)) = strip_segment_override(rhs) {
        let new_operands = format!("{lhs},{stripped}");
        let inner = parse_mov(&new_operands, line_no)?;
        return Ok(Instr::SegOverride { seg, inner: Box::new(inner) });
    }
    // Generic 16-bit reg-to-reg move (`mov bp,sp`, `mov sp,bp`,
    // `mov ax,dx`, `mov si,ax`, etc.). Tried before per-register
    // dispatch so it catches every reg-to-reg pair uniformly.
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::MovReg16Reg16 { dst, src });
    }
    // `mov ds, <reg16>` — copy a GP reg into DS. Used in interrupt
    // prologues (fixture 1655) to set DS = DGROUP after seeding the
    // value into BP via `mov bp, DGROUP`.
    if lhs == "ds"
        && let Some(src) = Reg16::parse(rhs)
    {
        return Ok(Instr::MovDsReg16 { reg: src });
    }
    // `mov <sreg>, word ptr [bp+disp]` — load a segment register
    // from memory. BCC's `_seg`-pointer codegen brings the segment
    // selector into ES at every dereference. Fixtures 4070–4073.
    if let Some(seg) = crate::ir::SegReg::parse(lhs)
        && let Some(offset) = parse_word_bp_relative(rhs)
    {
        return Ok(Instr::MovSregBpRel { seg, offset });
    }
    // `mov <reg16>, DGROUP` — load the DGROUP group's segment value
    // as a 16-bit immediate with a SegRelGroupTarget fixup. Fixture
    // 1655.
    if let Some(dst) = Reg16::parse(lhs)
        && rhs == "DGROUP"
    {
        return Ok(Instr::MovReg16Dgroup { reg: dst });
    }
    // `mov <reg16>, seg <segment-name>` — huge-model DS reload
    // prologue. The imm16 gets a SegBaseSegmentTarget FIXUPP so the
    // linker patches in the segment's paragraph value. Fixtures
    // 1770, 2057.
    if let Some(dst) = Reg16::parse(lhs)
        && let Some(seg_name) = rhs.strip_prefix("seg ")
    {
        return Ok(Instr::MovReg16SegBase {
            reg: dst,
            segment: seg_name.trim().to_string(),
        });
    }
    // `mov <reg16>, <segreg>` — segment-register to general-purpose
    // register copy (`mov dx, ds`, `mov dx, ss`). Used to form the
    // segment half of a far pointer before calling helpers that
    // take far pointers in DX:AX (e.g. `N_SPUSH@`). Fixture 420.
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), SegReg::parse(rhs)) {
        return Ok(Instr::MovReg16SegReg { dst, src });
    }
    // `mov word ptr [bp+disp], <segreg>` — store the segment half
    // of a far-pointer local. `8C` + ModR/M reg=<sreg>
    // r/m=110 (BP+disp). Fixtures 1649 / 1650 / 2058.
    if let Some(offset) = parse_word_bp_relative(lhs)
        && let Some(src) = SegReg::parse(rhs)
    {
        return Ok(Instr::MovBpRelSegReg { offset, src });
    }
    // `mov ax, es:[bx]` / `mov al, byte ptr es:[bx]` — far-pointer
    // word / byte read after `les bx`. Fixtures 1649 / 2058.
    if lhs == "ax" && (rhs == "es:[bx]" || rhs == "word ptr es:[bx]") {
        return Ok(Instr::MovAxEsBx);
    }
    if lhs == "al" && (rhs == "es:[bx]" || rhs == "byte ptr es:[bx]") {
        return Ok(Instr::MovAlEsBx);
    }
    // `mov ax, word ptr es:[bx+disp8]` — far-pointer indexed read.
    // Fixture 3958.
    if lhs == "ax"
        && let Some(inner) = rhs.strip_prefix("word ptr es:[bx+")
        && let Some(num) = inner.strip_suffix(']')
        && let Ok(disp) = num.parse::<u8>()
    {
        return Ok(Instr::MovAxEsBxDisp { disp });
    }
    if lhs == "al"
        && let Some(inner) = rhs.strip_prefix("byte ptr es:[bx+")
        && let Some(num) = inner.strip_suffix(']')
        && let Ok(disp) = num.parse::<u8>()
    {
        return Ok(Instr::MovAlEsBxDisp { disp });
    }
    // `mov es:[bx], ax/al/imm` — far-pointer write through the
    // ES:BX pair set up by `les bx`. Fixture 1650 (`*p = 99`).
    if (lhs == "es:[bx]" || lhs == "word ptr es:[bx]") && rhs == "ax" {
        return Ok(Instr::MovEsBxAx);
    }
    if (lhs == "es:[bx]" || lhs == "byte ptr es:[bx]") && rhs == "al" {
        return Ok(Instr::MovEsBxAl);
    }
    if lhs == "word ptr es:[bx]"
        && let Some(imm) = parse_imm16(rhs)
    {
        return Ok(Instr::MovEsBxImm16 { imm: imm as u16 });
    }
    if lhs == "byte ptr es:[bx]"
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::MovEsBxImm8 { imm });
    }
    // `mov {word|byte} ptr es:[bx+disp8], imm/ax/al` — far-pointer
    // indexed store. Used by `a[K] = v` writes through a stack-
    // resident far-pointer local. Fixture 1870.
    if let Some(disp) = parse_es_bx_disp(lhs, "word ptr es:[bx") {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovEsBxDispImm16 { disp, imm: imm as u16 });
        }
        if rhs == "ax" {
            return Ok(Instr::MovEsBxDispAx { disp });
        }
    }
    if let Some(disp) = parse_es_bx_disp(lhs, "byte ptr es:[bx") {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::MovEsBxDispImm8 { disp, imm });
        }
        if rhs == "al" {
            return Ok(Instr::MovEsBxDispAl { disp });
        }
    }
    // `mov bx,word ptr [bx]` — chain step for `**p` (fixture 195).
    if lhs == "bx" && rhs == "word ptr [bx]" {
        return Ok(Instr::MovBxFromBxPtr);
    }
    // `mov bx,word ptr [bx+disp8]` — chain step for nested arrow
    // access (fixture 1928).
    if lhs == "bx"
        && let Some(disp) = parse_word_bx_disp(rhs)
        && disp != 0
    {
        return Ok(Instr::MovBxBxDisp { disp });
    }
    // `mov dx,word ptr [si]` — low-half read of `*p` for `p: long *`
    // (fixture 309). Uses the shorter `8B 14` encoding (disp-less).
    if lhs == "dx" && rhs == "word ptr [si]" {
        return Ok(Instr::MovDxFromSiPtr);
    }
    // `mov dx,word ptr [si+disp8]` — high-half read of `*p` for
    // `p: long *` in the ABI return convention (fixture 351).
    if lhs == "dx" {
        if let Some(disp) = parse_word_si_disp(rhs) {
            if disp == 0 {
                return Ok(Instr::MovDxFromSiPtr);
            }
            return Ok(Instr::MovDxSiDisp { disp });
        }
        // `mov dx,word ptr [bx+disp8]` — DX gets the low half of
        // a long-pointer-subscript value before an inline shift
        // (fixture 904: `long *p; p[K] <<= N`).
        if let Some(disp) = parse_word_bx_disp(rhs)
            && disp != 0
        {
            return Ok(Instr::MovDxBxDisp { disp });
        }
    }
    if lhs == "ax" {
        if rhs == "word ptr [si]" {
            return Ok(Instr::MovAxFromSiPtr);
        }
        if rhs == "word ptr [bx]" {
            return Ok(Instr::MovAxFromBxPtr);
        }
        if rhs == "word ptr [bx+si]" {
            return Ok(Instr::MovAxFromBxSi);
        }
        // `mov ax,word ptr [si+disp8]` — high-half read of `*p`
        // for `p: long *` (fixture 309).
        if let Some(disp) = parse_word_si_disp(rhs) {
            return Ok(Instr::MovAxSiDisp { disp });
        }
        // `mov ax,word ptr [bx+disp8]` — load through BX at a
        // small offset (fixture 883: `int *p; p[K] *= y` loads
        // LHS into AX before the imul).
        if let Some(disp) = parse_word_bx_disp(rhs)
            && disp != 0
        {
            return Ok(Instr::MovAxBxDisp { disp });
        }
        // bx-indexed form first — `[bx]`/`[bx+K]` inside the symbol
        // part would otherwise get swallowed by `parse_group_symbol`.
        // Fixture 303 (`mov ax, word ptr DGROUP:_a[bx+2]`).
        if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(rhs) {
            return Ok(Instr::MovReg16GroupSymBxDisp {
                reg: Reg16::Ax,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovAxGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // Bare-symbol moffs16 load `mov ax, word ptr _g[+N]` — used
        // by huge-model code where DGROUP isn't in scope. Fixture
        // 2057.
        if let Some(symbol) = rhs.strip_prefix("word ptr ")
            && let Some(first) = symbol.chars().next()
            && (first == '_' || first == '@')
            && !symbol.contains(':')
            && !symbol.contains('[')
        {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovAxSym {
                symbol: sym.to_string(),
                offset,
            });
        }
    }
    // Bare-symbol AX-source store `mov word ptr _g, ax` — huge-
    // model write companion to `MovAxSym`. Fixture 3705.
    if rhs == "ax"
        && let Some(symbol) = lhs.strip_prefix("word ptr ")
        && let Some(first) = symbol.chars().next()
        && (first == '_' || first == '@')
        && !symbol.contains(':')
        && !symbol.contains('[')
    {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::MovSymAx {
            symbol: sym.to_string(),
            offset,
        });
    }
    // Bare-symbol immediate store `mov word ptr _g, imm16` —
    // huge-model immediate write. Fixture 3704.
    if let Some(symbol) = lhs.strip_prefix("word ptr ")
        && let Some(first) = symbol.chars().next()
        && (first == '_' || first == '@')
        && !symbol.contains(':')
        && !symbol.contains('[')
        && let Some(imm) = parse_imm16(rhs)
    {
        let (sym, offset) = split_sym_offset(symbol);
        return Ok(Instr::MovSymImm16 {
            symbol: sym.to_string(),
            offset,
            imm: imm as u16,
        });
    }
    // `mov <reg16>,word ptr <group>:<sym>[+N]` — generic disp16 load
    // for non-AX destinations (AX has the shorter A1 path above).
    // Fixture 192: `mov bx,word ptr DGROUP:_p`.
    if let Some(reg) = Reg16::parse(lhs) {
        // bx-indexed form first — it has a `[` in the symbol part
        // that `parse_group_symbol` would otherwise swallow.
        // Fixture 303 (`mov ax, word ptr DGROUP:_a[bx+2]`).
        if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(rhs) {
            return Ok(Instr::MovReg16GroupSymBxDisp {
                reg,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
        // SI-indexed form for variable-indexed int-array reads.
        if let Some((group, symbol, disp)) = parse_group_symbol_base_disp(rhs, "si", "word") {
            return Ok(Instr::MovReg16GroupSymSiDisp {
                reg,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovReg16WordGroupSym {
                reg,
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `mov <reg16>, word ptr [si]` and `[si+disp8]` — chained-
        // pointer indirection (fixture 2816's `mov bx, [si]`).
        if rhs == "word ptr [si]" {
            return Ok(Instr::MovReg16FromSiPtr { reg });
        }
        if let Some(disp) = parse_word_si_disp(rhs) {
            return Ok(Instr::MovReg16SiDisp { reg, disp });
        }
        // `mov <reg16>, word ptr [di]` and `[di+disp8]` — sibling
        // for the DI-bound pointer (fixture 2495).
        if rhs == "word ptr [di]" {
            return Ok(Instr::MovReg16FromDiPtr { reg });
        }
        if let Some(disp) = parse_word_di_disp(rhs) {
            return Ok(Instr::MovReg16DiDisp { reg, disp });
        }
        // `mov <reg16>, word ptr [bx]` — BX sibling. The dedicated
        // AX/BX variants above (`MovAxFromBxPtr`, `MovBxFromBxPtr`)
        // already short-circuit those two destinations; this catches
        // the remaining registers (e.g. DI for `**pp` final reads,
        // fixture 4227).
        if rhs == "word ptr [bx]" {
            return Ok(Instr::MovReg16FromBxPtr { reg });
        }
    }
    // Generic 16-bit `mov <reg>,offset <group>:<sym>` (fixtures 108
    // for AX, 157 for SI). Tried before reg-imm so it doesn't get
    // shadowed by a misparse of `offset` as a label.
    if let Some(reg) = Reg16::parse(lhs) {
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovReg16OffsetGroupSym {
                reg,
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `mov <reg>,offset <code-symbol>` — symbol with no group
        // prefix, i.e. an intra-segment code label.
        if let Some(symbol) = parse_offset_symbol(rhs) {
            return Ok(Instr::MovReg16OffsetSym {
                reg,
                symbol: symbol.to_string(),
            });
        }
        // Generic 16-bit bp-relative load: `mov ax,[bp-2]`,
        // `mov bx,[bp-2]`, etc. Tried after the AX-specific
        // group-symbol path above so that `mov ax,word ptr DGROUP:_x`
        // routes through MovAxGroupSym, not here.
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::MovReg16BpRel { reg, offset });
        }
    }
    // Generic 16-bit reg-imm load: `mov si,10`, `mov ax,42`, etc.
    // Tried after the AX-specific paths above so that a reg-imm
    // pattern doesn't shadow them.
    if let Some(reg) = Reg16::parse(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovReg16Imm { reg, imm });
        }
    }
    if lhs == "al" {
        if rhs == "byte ptr [si]" {
            return Ok(Instr::MovAlFromSiPtr);
        }
        if rhs == "byte ptr [bx]" {
            return Ok(Instr::MovAlFromBxPtr);
        }
        if rhs == "byte ptr [di]" {
            return Ok(Instr::MovAlFromDiPtr);
        }
        if rhs == "byte ptr [bx+si]" {
            return Ok(Instr::MovAlFromBxSi);
        }
        if rhs == "byte ptr [bx+di]" {
            return Ok(Instr::MovAlFromBxDi);
        }
        // `mov al, byte ptr [<imm16>]` — moffs8 load from a literal
        // 16-bit address. Used (with a wrapping ES override) by
        // `_seg`-pointer deref with a constant offset. Fixtures
        // 4070 (offset 0), 4071 (offset 5).
        if let Some(inner) = rhs.strip_prefix("byte ptr [")
            && let Some(num) = inner.strip_suffix(']')
            && let Some(addr) = parse_imm16(num)
        {
            return Ok(Instr::MovAlAtAddr { addr });
        }
        // `mov al, byte ptr [bp+si+disp]` — char-array load via
        // BP+SI indexed addressing. Fixture 2488.
        if let Some(disp) = parse_byte_bp_si_disp(rhs) {
            return Ok(Instr::MovAlBpSiDisp { disp });
        }
        // Bare-symbol moffs8 load `mov al, byte ptr _g[+N]` — the
        // byte-width sibling of `MovAxSym`. Used by huge-model
        // char-global reads. Fixture 3698.
        if let Some(symbol) = rhs.strip_prefix("byte ptr ")
            && let Some(first) = symbol.chars().next()
            && (first == '_' || first == '@')
            && !symbol.contains(':')
            && !symbol.contains('[')
        {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovAlSym {
                symbol: sym.to_string(),
                offset,
            });
        }
        // `mov al,byte ptr [bx+disp8]` — char-pointer subscript
        // load (fixture 865). disp=0 stays with `MovAlFromBxPtr`
        // (2-byte form).
        if let Some(disp) = parse_byte_bx_disp(rhs)
            && disp != 0
        {
            return Ok(Instr::MovAlBxDisp { disp });
        }
        // `mov al, byte ptr <group>:<sym>[bx]` — bx-indexed load.
        // Tried before the plain group-symbol form. Fixture 2613.
        if let Some((group, symbol, disp)) = parse_byte_group_symbol_bx_disp(rhs) {
            return Ok(Instr::MovReg8GroupSymBxDisp {
                reg: Reg8::Al,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
        // `mov al, byte ptr <group>:<sym>[si]` — si-indexed load.
        // Fixture 1426 (`dst[i] = src[i]` with i in SI).
        if let Some((group, symbol, disp)) = parse_group_symbol_base_disp(rhs, "si", "byte") {
            return Ok(Instr::MovReg8GroupSymSiDisp {
                reg: Reg8::Al,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
        // `mov al,byte ptr DGROUP:_g` — 8-bit moffs8 load.
        if let Some((group, symbol)) = parse_byte_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovAlGroupSym {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
    }
    // Generic 8-bit register operand on the LHS.
    if let Some(reg) = Reg8::parse(lhs) {
        if let Some(offset) = parse_byte_bp_relative(rhs) {
            return Ok(Instr::MovReg8BpRel { reg, offset });
        }
        if let Some(src) = Reg8::parse(rhs) {
            return Ok(Instr::MovReg8Reg8 { dst: reg, src });
        }
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::MovReg8Imm8 { reg, imm });
        }
        // BX-indexed byte load: `mov <reg8>, byte ptr <group>:<sym>[bx]`
        // — variable-indexed char-array read (fixture 2613).
        if let Some((group, symbol, disp)) = parse_byte_group_symbol_bx_disp(rhs) {
            return Ok(Instr::MovReg8GroupSymBxDisp {
                reg,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
        // `mov <reg8>, byte ptr <group>:<sym>` for non-AL dst
        // (fixture 739: `mov cl, byte ptr DGROUP:_h`).
        if let Some((group, symbol)) = parse_byte_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovReg8GroupSym {
                reg,
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // SI-indexed byte load (variable-indexed char-array read).
        if let Some((group, symbol, disp)) = parse_group_symbol_base_disp(rhs, "si", "byte") {
            return Ok(Instr::MovReg8GroupSymSiDisp {
                reg,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
    }
    // LHS `word ptr [bp+N]` — int-width stack store. We *require*
    // the explicit `word ptr` prefix here so that `mov byte ptr
    // [bp+N],1` doesn't route to the word-store path (which would
    // emit a 5-byte C7 encoding instead of the correct 4-byte C6).
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovBpRelImm { offset, imm });
        }
        // `mov word ptr [bp-N],offset _f` — store function/data
        // symbol's address into stack local (fixture 110).
        if let Some(sym) = parse_offset_symbol(rhs) {
            return Ok(Instr::MovBpRelOffsetSym {
                offset,
                symbol: sym.to_string(),
            });
        }
        // `mov word ptr [bp-N], offset DGROUP:_g` — store group-
        // qualified symbol offset into stack local (fixture 601's
        // `p = &g;` peephole).
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            let (sym, sym_offset) = split_sym_offset(symbol);
            return Ok(Instr::MovBpRelOffsetGroupSym {
                offset,
                group: group.to_string(),
                symbol: sym.to_string(),
                sym_offset,
            });
        }
        // `mov word ptr [bp-N],ax` — store AX. Fixture 160 uses this
        // to stash the switch scrutinee into a stack slot before the
        // linear-search loop walks it.
        if rhs == "ax" {
            return Ok(Instr::MovBpRelAx { offset });
        }
        // `mov word ptr [bp-N], <reg16>` for non-AX sources
        // (fixture 286 stores DX as the low half of a long local).
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::MovBpRelReg16 { offset, reg });
        }
    }
    // `mov ax,word ptr cs:[bx]` — CS-override load through BX. No
    // displacement. Fixture 160's value-table walk.
    if lhs == "ax" && rhs == "word ptr cs:[bx]" {
        return Ok(Instr::MovAxFromCsBx);
    }
    // `mov ax,word ptr cs:[bx+disp8]` — long-linear-search reads
    // the case-high half from the +2*N offset into the table while
    // BX still points to the case-low. Fixture 1913.
    if lhs == "ax"
        && let Some(disp) = rhs.strip_prefix("word ptr cs:[bx+").and_then(|s| s.strip_suffix(']'))
        && let Ok(d) = disp.parse::<u8>()
    {
        return Ok(Instr::MovAxFromCsBxDisp { disp: d });
    }
    // LHS `byte ptr [bp+N]` — 8-bit stack store.
    if let Some(offset) = parse_byte_bp_relative(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::MovBpRelImm8 { offset, imm });
        }
        if let Some(src) = Reg8::parse(rhs) {
            return Ok(Instr::MovBpRelReg8 { offset, reg: src });
        }
    }
    // LHS `word ptr [si]` — store through SI pointer (fixture 136).
    if lhs == "word ptr [si]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovSiPtrImm { imm });
        }
        if let Some(src) = Reg16::parse(rhs) {
            return Ok(Instr::MovSiPtrReg16 { src });
        }
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            let (sym, sym_offset) = split_sym_offset(symbol);
            return Ok(Instr::MovDerefRegOffsetGroupSym {
                reg: Reg16::Si,
                group: group.to_string(),
                symbol: sym.to_string(),
                sym_offset,
            });
        }
    }
    // LHS `word ptr [di]` — store-through-DI variant of the above.
    if lhs == "word ptr [di]" {
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            let (sym, sym_offset) = split_sym_offset(symbol);
            return Ok(Instr::MovDerefRegOffsetGroupSym {
                reg: Reg16::Di,
                group: group.to_string(),
                symbol: sym.to_string(),
                sym_offset,
            });
        }
    }
    // LHS `word ptr [bx]` — store-through-BX variant.
    if lhs == "word ptr [bx]" {
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            let (sym, sym_offset) = split_sym_offset(symbol);
            return Ok(Instr::MovDerefRegOffsetGroupSym {
                reg: Reg16::Bx,
                group: group.to_string(),
                symbol: sym.to_string(),
                sym_offset,
            });
        }
    }
    // LHS `byte ptr [si+disp]` — byte store through SI pointer
    // (fixture 1016: char-pointer subscript write).
    if let Some(disp) = parse_byte_si_disp(lhs)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::MovByteSiDispImm8 { disp: i16::from(disp), imm: imm as u8 });
    }
    // RHS `byte ptr [si+disp]` — byte load through SI pointer into an
    // 8-bit register (fixture 1019: char-pointer subscript read).
    if let Some(reg) = Reg8::parse(lhs)
        && let Some(disp) = parse_byte_si_disp(rhs)
    {
        return Ok(Instr::MovReg8ByteSiDisp { reg, disp: i16::from(disp) });
    }
    // LHS `word ptr [di]` — store through DI pointer (fixture 628).
    if lhs == "word ptr [di]" {
        if let Some(src) = Reg16::parse(rhs) {
            return Ok(Instr::MovDiPtrReg16 { src });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovDiPtrImm { imm });
        }
    }
    // LHS `byte ptr [bx]` — byte-store through BX pointer
    // (fixture 3559: `buf[i] = 0` for char* — bx is the
    // post-scaling indexed address).
    if lhs == "byte ptr [bx]"
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::MovBxPtrImm8 { imm: imm as u8 });
    }
    // LHS `byte ptr [bx+si]` / `[bx+di]` — indexed byte store
    // (fixture 3559: BCC folds the array-base + index into a
    // single memory operand instead of computing the address
    // into BX upfront).
    if lhs == "byte ptr [bx+si]"
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::MovBxSiPtrImm8 { imm: imm as u8 });
    }
    if lhs == "byte ptr [bx+di]"
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::MovBxDiPtrImm8 { imm: imm as u8 });
    }
    // LHS `byte ptr [si]` — byte-store through SI pointer (fixture 465).
    if lhs == "byte ptr [si]" {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::MovSiPtrImm8 { imm: imm as u8 });
        }
        // `mov byte ptr [si], <reg8>` — char-via-SI pointer store
        // (fixture 710: writeback of `p->c += K`).
        if let Some(src) = Reg8::parse(rhs) {
            return Ok(Instr::MovSiPtrReg8 { src });
        }
    }
    // LHS `byte ptr [di]` — char-store through DI pointer.
    // Fixture 3529.
    if lhs == "byte ptr [di]"
        && let Some(src) = Reg8::parse(rhs)
    {
        return Ok(Instr::MovDiPtrReg8 { src });
    }
    // LHS `byte ptr [<imm16>]` — immediate-to-memory byte store at
    // a literal address. Used (with a wrapping ES override) by
    // `_seg`-pointer write with a constant offset. Fixture 4072.
    if let Some(inner) = lhs.strip_prefix("byte ptr [")
        && let Some(num) = inner.strip_suffix(']')
        && let Some(addr) = parse_imm16(num)
        && let Some(imm) = parse_imm8(rhs)
    {
        return Ok(Instr::MovByteAtAddrImm8 { addr, imm });
    }
    // LHS `word ptr [si+disp]` — store-imm to long pointer's high
    // half (fixture 308: `*p = K` where `p: long *` in SI emits
    // `mov word ptr [si+2], <high>` after the low-half partner).
    if let Some(disp) = parse_word_si_disp(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovSiDispImm { disp, imm });
        }
        // Reg-source variant: `p->y = z` writes a register through
        // SI at a field offset. Fixture 1955.
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::MovSiDispReg16 { disp, reg });
        }
    }
    // LHS `word ptr [bx]` — store through BX pointer (fixture 144).
    if lhs == "word ptr [bx]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovBxPtrImm { imm });
        }
        if rhs == "ax" {
            return Ok(Instr::MovBxPtrAx);
        }
        // Non-AX register store. Fixture 2244 (`arr[i] = i` with i
        // in SI → `mov [bx], si`).
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::MovBxPtrReg16 { reg });
        }
    }
    // LHS `byte ptr [bx]` with AL — char-element store after a
    // BX-indexed address compute (fixture 1219).
    if lhs == "byte ptr [bx]" && rhs == "al" {
        return Ok(Instr::MovBxPtrAl);
    }
    // LHS `byte ptr [bx+disp8]` — byte-store through BX pointer at
    // a small offset, used by `char *p; p[K] op= …` (fixture 865).
    // disp=0 has no fixture yet so the 2-byte form is deferred.
    if rhs == "al"
        && let Some(disp) = parse_byte_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::MovBxDispAl { disp });
    }
    // LHS `word ptr [bx+disp8]` — word-store through BX pointer
    // (fixture 883: `int *p; p[K] *= y` store-back step).
    if rhs == "ax"
        && let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::MovBxDispAx { disp });
    }
    // `mov word ptr [bx+disp8], dx` — DX-result store (fixture
    // 884: `int *p; p[K] %= y` writes the idiv remainder).
    if rhs == "dx"
        && let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
    {
        return Ok(Instr::MovBxDispDx { disp });
    }
    // `mov word ptr [bx+disp8], imm16` — const word store through
    // BX (fixture 897: `long *p; p[K] = v` writes both halves).
    if let Some(disp) = parse_word_bx_disp(lhs)
        && disp != 0
        && let Some(imm) = parse_imm16(rhs)
    {
        return Ok(Instr::MovBxDispImm { disp, imm: imm as u16 });
    }
    // LHS `word ptr <group>:<sym>[bx+disp]` — store immediate to a
    // data-segment global through bx-indexed addressing. Used by
    // variable-indexed long-array writes (fixture 305). Tried before
    // the plain `<group>:<sym>+N` path since `[bx]` in the symbol
    // part would otherwise get swallowed.
    if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovGroupSymBxDispImm {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm: imm as u16,
            });
        }
        // `mov word ptr <group>:<sym>[bx+disp], <reg16>` — bx-indexed
        // store of a register. Fixture 510 (`a[i] = i` with `i` in SI).
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::MovGroupSymBxDispReg16 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                reg,
            });
        }
    }
    // Byte sibling: `mov byte ptr <group>:<sym>[bx+disp], ...` for
    // var-indexed char-array writes (fixture 2613).
    if let Some((group, symbol, disp)) = parse_byte_group_symbol_bx_disp(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::MovGroupSymBxDispImm8 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm: imm as u8,
            });
        }
        if let Some(reg) = Reg8::parse(rhs) {
            return Ok(Instr::MovGroupSymBxDispReg8 {
                reg,
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
            });
        }
    }
    // LHS `byte ptr <group>:<sym>[si+disp]` — SI-indexed byte store
    // for variable-indexed char-array writes (fixture 1366: `buf[i]
    // = 'X'`).
    if let Some((group, symbol, disp)) = parse_group_symbol_base_disp(lhs, "si", "byte") {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::MovGroupSymSiDispByteImm8 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm: imm as u8,
            });
        }
        if let Some(reg) = Reg8::parse(rhs) {
            return Ok(Instr::MovGroupSymSiDispReg8 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                reg,
            });
        }
    }
    // LHS `word ptr <group>:<sym>[si+disp]` — SI-indexed word
    // store for variable-indexed int-array writes.
    if let Some((group, symbol, disp)) = parse_group_symbol_base_disp(lhs, "si", "word") {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovGroupSymSiDispImm16 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                imm: imm as u16,
            });
        }
        if let Some(reg) = Reg16::parse(rhs) {
            return Ok(Instr::MovGroupSymSiDispReg16 {
                group: group.to_string(),
                symbol: symbol.to_string(),
                disp,
                reg,
            });
        }
    }
    // LHS `byte ptr <group>:<sym>[+N]` — store immediate byte to a
    // data-segment global. Fixture 449 (`c = 'A'` for char global).
    if let Some((group, symbol)) = parse_byte_group_symbol(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovGroupSymImm8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm: imm as u8,
            });
        }
        // `mov byte ptr <group>:<sym>, al` — moffs8 store short
        // form (`A2 lo hi`). Used by the char-global compound-with-
        // constant load-modify-store pattern (fixture 683).
        if rhs == "al" {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovGroupSymAl {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        // `mov byte ptr <group>:<sym>, <reg8>` (non-AL) — generic
        // 88-form store. Char-global `%= K` stores DL back (fixture
        // 692).
        if let Some(reg) = Reg8::parse(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovGroupSymReg8 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    // LHS `word ptr <group>:<sym>[+N]`, RHS `offset <group>:<sym>`
    // — store the address of one global into another. Fixture 480
    // (`p = &x;` between two globals). Try before the imm16 path so
    // the `offset` keyword isn't mis-parsed as a stray label.
    if let Some((dst_group, dst_symbol)) = parse_group_symbol(lhs) {
        if let Some((src_group, src_symbol)) = parse_offset_group_symbol(rhs) {
            let (dst_sym, dst_offset) = split_sym_offset(dst_symbol);
            let (src_sym, src_offset) = split_sym_offset(src_symbol);
            return Ok(Instr::MovGroupSymOffsetGroupSym {
                dst_group: dst_group.to_string(),
                dst_symbol: dst_sym.to_string(),
                dst_offset,
                src_group: src_group.to_string(),
                src_symbol: src_sym.to_string(),
                src_offset,
            });
        }
    }
    // LHS `word ptr <group>:<sym>[+N]` — store immediate to a
    // data-segment global. Fixture 205 (`long g = K;` writes two
    // halves: `_g+2` for the high word, `_g` for the low word).
    if let Some((group, symbol)) = parse_group_symbol(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovGroupSymImm16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                imm: imm as u16,
            });
        }
        // Store AX → moffs16 short form (A3). Fixture 207 writes
        // long-high via AX. Other regs (non-AX) take the generic
        // `89 (modrm) ...` path below.
        if rhs == "ax" {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovGroupSymAx {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
        }
        if let Some(reg) = Reg16::parse(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovGroupSymReg16 {
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
                reg,
            });
        }
    }
    Err(AsmError::new(
        line_no,
        format!("mov: unsupported operand form `{operands}`"),
    ))
}
/// Generic `<op> word ptr [bp+<offset>]` parser — single-operand
/// instructions like `imul`, `idiv`, `mul`, `div`, `neg`, `not`.
pub(crate) fn parse_single_op_word_ptr(
    operands: &str,
    line_no: usize,
    op_name: &str,
    make: impl FnOnce(i16) -> Instr,
) -> AsmResult<Instr> {
    if let Some(offset) = parse_bp_relative(operands) {
        return Ok(make(offset));
    }
    Err(AsmError::new(
        line_no,
        format!("{op_name}: unsupported operand form `{operands}`"),
    ))
}
/// Generic `<op> ax,word ptr [bp+<offset>]` parser. The four ALU
/// opcodes (add/sub/and/or/xor/cmp) share the same operand shape;
/// only the resulting IR variant differs.
pub(crate) fn parse_alu_ax_mem(
    operands: &str,
    line_no: usize,
    op_name: &str,
    make: impl FnOnce(i16) -> Instr,
) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("{op_name}: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(make(offset));
        }
    }
    Err(AsmError::new(
        line_no,
        format!("{op_name}: unsupported operand form `{operands}`"),
    ))
}
