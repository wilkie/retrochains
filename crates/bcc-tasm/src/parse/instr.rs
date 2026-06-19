use super::*;

pub(crate) fn parse_segment_attrs(rest: &str, line_no: usize) -> AsmResult<(SegAlign, SegCombine, String)> {
    // rest looks like: `byte public 'CODE'` or `word public 'DATA'`
    let mut it = rest.split_whitespace();
    let align_tok = it.next().unwrap_or("");
    let combine_tok = it.next().unwrap_or("");
    let class_tok = it.collect::<Vec<_>>().join(" ");
    let align = match align_tok {
        "byte" => SegAlign::Byte,
        "word" => SegAlign::Word,
        other => {
            return Err(AsmError::new(
                line_no,
                format!("segment: unrecognized alignment `{other}`"),
            ))
        }
    };
    let combine = match combine_tok {
        "public" => SegCombine::Public,
        other => {
            return Err(AsmError::new(
                line_no,
                format!("segment: unrecognized combine `{other}`"),
            ))
        }
    };
    let class = unquote(&class_tok).unwrap_or(&class_tok).to_string();
    Ok((align, combine, class))
}
pub(crate) fn parse_instr(line: &Line<'_>) -> AsmResult<Instr> {
    let kw = line.keyword;
    let rest = line.rest.trim_end();
    match kw {
        "pushf" if rest.is_empty() => Ok(Instr::Pushf),
        "push" => {
            if rest == "ds" {
                return Ok(Instr::PushDs);
            }
            if rest == "ss" {
                return Ok(Instr::PushSs);
            }
            if rest == "cs" {
                return Ok(Instr::PushCs);
            }
            if rest == "es" {
                return Ok(Instr::PushEs);
            }
            // `push <imm8sx>` — 186+ sign-extended-byte push. The
            // operand is a decimal/signed literal that fits in i8.
            if let Some(imm) = parse_imm8_signed(rest) {
                return Ok(Instr::PushImm8Sx { imm });
            }
            if let Some(reg) = Reg16::parse(rest) {
                return Ok(Instr::PushReg16 { reg });
            }
            // `push word ptr <group>:<sym>[+N]` — memory push of a
            // long-arith helper argument (fixture 232).
            if let Some((group, symbol)) = parse_group_symbol(rest) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::PushGroupSym {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `push word ptr [bp+N]` — long-arg push from a stack
            // local (fixture 323).
            if let Some(offset) = parse_word_bp_relative(rest) {
                return Ok(Instr::PushBpRel { offset });
            }
            // `push word ptr [si]` / `push word ptr [si+K]` —
            // long-arg push through a pointer (fixture 325).
            if rest == "word ptr [si]" {
                return Ok(Instr::PushSiPtr);
            }
            if let Some(disp) = parse_word_si_disp(rest) {
                if disp == 0 {
                    return Ok(Instr::PushSiPtr);
                }
                return Ok(Instr::PushSiDisp { disp });
            }
            // `push word ptr [bx+disp8]` — memory-operand push
            // peephole on a global-pointer subscript arg (fixture
            // 893). disp=0 has no fixture yet — left for a future
            // `PushBxPtr` 2-byte form when one demands it.
            if let Some(disp) = parse_word_bx_disp(rest)
                && disp != 0
            {
                return Ok(Instr::PushBxDisp { disp });
            }
            Err(AsmError::new(line.line_no, format!("push: unsupported operand `{rest}`")))
        }
        "pop" => {
            if rest == "es" {
                return Ok(Instr::PopEs);
            }
            if rest == "ds" {
                return Ok(Instr::PopDs);
            }
            Reg16::parse(rest)
                .map(|reg| Instr::PopReg16 { reg })
                .ok_or_else(|| AsmError::new(line.line_no, format!("pop: unsupported operand `{rest}`")))
        }
        "iret" if rest.is_empty() => Ok(Instr::Iret),
        "mov" => parse_mov(rest, line.line_no),
        // Generic ALU forms `<op> ax,word ptr [bp+N]`. Some opcodes
        // also have special operand forms (`sub sp,imm`, `xor ax,ax`)
        // that take precedence — handled in the per-op parser below.
        "xchg" => {
            // `xchg <reg8>, <reg8>` — swap two byte registers,
            // used by inline asm (`asm xchg ah, al`, fixture 2122).
            let (lhs, rhs) = split_comma(rest).ok_or_else(|| {
                AsmError::new(line.line_no, format!("xchg: expected `lhs,rhs`, got {rest:?}"))
            })?;
            if let (Some(dst), Some(src)) = (Reg8::parse(lhs), Reg8::parse(rhs)) {
                return Ok(Instr::XchgReg8Reg8 { dst, src });
            }
            return Err(AsmError::new(
                line.line_no,
                format!("xchg: unsupported operand form `{rest}`"),
            ));
        }
        "add" => parse_add(rest, line.line_no),
        "adc" => parse_adc(rest, line.line_no),
        "sub" => parse_sub(rest, line.line_no),
        "sbb" => parse_sbb(rest, line.line_no),
        "and" => parse_and(rest, line.line_no),
        "or" => parse_or(rest, line.line_no),
        "xor" => parse_xor(rest, line.line_no),
        "cmp" => parse_cmp(rest, line.line_no),
        "test" => {
            // Forms:
            //   `test word ptr <group>:<sym>[+N], imm16` — global bit
            //   test (fixture 569).
            //   `test <reg16>, imm16` — register bit test (fixture
            //   1415, popcount `if (x & 1)` with x in SI).
            let (lhs, rhs) = split_comma(rest).ok_or_else(|| {
                AsmError::new(line.line_no, format!("test: expected `lhs,rhs`, got {rest:?}"))
            })?;
            if let Some((group, symbol)) = parse_group_symbol(lhs) {
                if let Some(imm) = parse_imm16(rhs) {
                    let (sym, offset) = split_sym_offset(symbol);
                    return Ok(Instr::TestGroupSymImm16 {
                        group: group.to_string(),
                        symbol: sym.to_string(),
                        offset,
                        imm,
                    });
                }
            }
            if let Some(reg) = Reg16::parse(lhs)
                && let Some(imm) = parse_imm16(rhs)
            {
                return Ok(Instr::TestReg16Imm16 { reg, imm });
            }
            // `test <dst-reg>, <src-reg>` — register-to-register
            // bit test. Fixture 3452.
            if let Some(dst) = Reg16::parse(lhs)
                && let Some(src) = Reg16::parse(rhs)
            {
                return Ok(Instr::TestReg16Reg16 { dst, src });
            }
            // `test word ptr [bp+disp8], imm16` — local bit test
            // (fixture 1853).
            if let Some(offset) = parse_word_bp_relative(lhs)
                && let Some(imm) = parse_imm16(rhs)
            {
                return Ok(Instr::TestBpRelImm16 { offset, imm: imm as u16 });
            }
            // `test word ptr [bp+disp8], ax` — local mem vs reg
            // (fixture 3539, `(x & mask) != 0`).
            if let Some(offset) = parse_word_bp_relative(lhs)
                && rhs == "ax"
            {
                return Ok(Instr::TestBpRelAx { offset });
            }
            // `test ax, word ptr [bp+disp]` — operand-swapped form
            // of the above. TEST r16, r/m16 (85 ModR/M) is symmetric
            // with TEST r/m16, r16, so the byte encoding is
            // identical; the assembly spelling differs and we route
            // it through the same instruction. Fixture 2399
            // (`if (mask & (1 << i))` for stack-resident `mask`).
            if lhs == "ax"
                && let Some(offset) = parse_word_bp_relative(rhs)
            {
                return Ok(Instr::TestBpRelAx { offset });
            }
            Err(AsmError::new(
                line.line_no,
                format!("test: unsupported operand form `{rest}`"),
            ))
        }
        "imul" => {
            // Five forms: `imul word ptr [bp+N]` (BpRel),
            // `imul byte ptr [bp+N]` (byte form, fixture 672),
            // `imul word ptr <group>:<sym>` (GroupSym, fixture 809),
            // `imul word ptr [si]` (SiPtr, fixture 824 sibling),
            // and `imul <reg16>` (single reg operand, fixture 155).
            if let Some(reg) = Reg16::parse(rest) {
                return Ok(Instr::ImulReg16 { reg });
            }
            if let Some(offset) = parse_byte_bp_relative(rest) {
                return Ok(Instr::ImulByteBpRel { offset });
            }
            if rest == "word ptr [si]" {
                return Ok(Instr::ImulSiPtr);
            }
            if let Some((group, symbol)) = parse_group_symbol(rest) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::ImulGroupSym {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            parse_single_op_word_ptr(rest, line.line_no, "imul", |o| Instr::ImulBpRel { offset: o })
        }
        "idiv" => {
            if let Some(reg) = Reg16::parse(rest) {
                return Ok(Instr::IdivReg16 { reg });
            }
            if let Some(offset) = parse_byte_bp_relative(rest) {
                return Ok(Instr::IdivByteBpRel { offset });
            }
            if rest == "word ptr [si]" {
                return Ok(Instr::IdivSiPtr);
            }
            if let Some((group, symbol)) = parse_group_symbol(rest) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::IdivGroupSym {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            parse_single_op_word_ptr(rest, line.line_no, "idiv", |o| Instr::IdivBpRel { offset: o })
        }
        "div" => {
            // Unsigned byte div with explicit `al,` accumulator
            // (BCC's TASM listing for unsigned char compound `/=`
            // / `%=`, fixture 677): `div al,byte ptr [bp+N]`.
            if let Some(stripped) = rest.strip_prefix("al,") {
                if let Some(offset) = parse_byte_bp_relative(stripped.trim_start()) {
                    return Ok(Instr::DivByteBpRel { offset });
                }
            }
            // Unsigned 16-bit divide of DX:AX by a register. Sibling
            // of `IdivReg16`. Fixture 948 (`unsigned a; return a / 7`
            // → `mov bx, 7; xor dx, dx; div bx`).
            if let Some(reg) = Reg16::parse(rest) {
                return Ok(Instr::DivReg16 { reg });
            }
            // Unsigned 16-bit divide of DX:AX by a stack local.
            // Sibling of `IdivBpRel`. Fixture 946.
            parse_single_op_word_ptr(rest, line.line_no, "div", |o| Instr::DivBpRel { offset: o })
        }
        "cwd" => Ok(Instr::Cwd),
        "cbw" => Ok(Instr::Cbw),
        "lea" => parse_lea(rest, line.line_no),
        "les" => {
            // `les bx, word ptr [bp+disp]` — load far pointer (4 bytes
            // at the bp-relative slot) into ES:BX. Used to bring a
            // 4-byte far-pointer local into ES:BX before a deref or
            // store. Fixtures 1649, 1650, 1651, 2058.
            let (lhs, rhs) = split_comma(rest).ok_or_else(|| {
                AsmError::new(line.line_no, format!("les: expected `lhs,rhs`, got {rest:?}"))
            })?;
            if lhs == "bx"
                && let Some(offset) = parse_word_bp_relative(rhs)
            {
                return Ok(Instr::LesBxBpRel { offset });
            }
            // `les bx, dword ptr [bp+disp]` — same encoding as the
            // word-ptr form (`les` always loads 4 bytes), accepted
            // as a textual variant. Fixture 3958.
            if lhs == "bx"
                && let Some(offset) = parse_dword_bp_relative(rhs)
            {
                return Ok(Instr::LesBxBpRel { offset });
            }
            // `les bx, dword ptr <group>:<sym>[+N]` — disp16 form
            // used to load a file-scope far-pointer global into
            // ES:BX. Fixtures 3760 / 3761.
            if lhs == "bx"
                && let Some(after) = rhs.strip_prefix("dword ptr ")
                && let Some((group, sym_part)) = after.split_once(':')
            {
                let (sym, offset) = split_sym_offset(sym_part.trim());
                return Ok(Instr::LesBxGroupSym {
                    group: group.trim().to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `les bx, dword ptr <sym>[+N]` — bare-symbol form used
            // by huge-model `*p` when `p` is a file-scope far
            // pointer. Fixture 3902.
            if lhs == "bx"
                && let Some(symbol) = rhs.strip_prefix("dword ptr ")
                && let Some(first) = symbol.chars().next()
                && (first == '_' || first == '@')
                && !symbol.contains(':')
                && !symbol.contains('[')
            {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::LesBxSym {
                    symbol: sym.to_string(),
                    offset,
                });
            }
            Err(AsmError::new(line.line_no, format!("les: unsupported operand form `{rest}`")))
        }
        "neg" => Reg16::parse(rest)
            .map(|reg| Instr::NegReg16 { reg })
            .ok_or_else(|| AsmError::new(line.line_no, format!("neg: bad register `{rest}`"))),
        "not" => Reg16::parse(rest)
            .map(|reg| Instr::NotReg16 { reg })
            .ok_or_else(|| AsmError::new(line.line_no, format!("not: bad register `{rest}`"))),
        "shl" if rest == "ax,cl" => Ok(Instr::ShlAxCl),
        "sar" if rest == "ax,cl" => Ok(Instr::SarAxCl),
        "shr" if rest == "ax,cl" => Ok(Instr::ShrAxCl),
        "shl" if rest.ends_with(",cl") => {
            // Variable-count shift on any 16-bit reg (fixture 537:
            // `mov cl, 4; shl si, cl` for `int x; x <<= 4`). Also
            // routes 8-bit variants (fixture 670 sibling: `shl dl,
            // cl`) — Reg8 is tried first since it doesn't overlap
            // with any Reg16 name. Memory-direct byte-global form
            // (fixture 697: `shl byte ptr DGROUP:_g, cl`) and word
            // form (fixture 805: `shl word ptr DGROUP:_g, cl`)
            // follow.
            let r = rest.strip_suffix(",cl").unwrap_or(rest);
            if let Some(reg) = Reg8::parse(r) {
                return Ok(Instr::ShlReg8Cl { reg });
            }
            if r == "word ptr [si]" {
                return Ok(Instr::ShlSiPtrCl);
            }
            if let Some((group, symbol)) = parse_byte_group_symbol(r) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::ShlGroupSymByteCl {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            if let Some((group, symbol)) = parse_group_symbol(r) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::ShlGroupSymCl {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `shl word ptr [bx+disp8], cl` — pointer-subscript
            // variable shift (fixture 882).
            if let Some(disp) = parse_word_bx_disp(r)
                && disp != 0
            {
                return Ok(Instr::ShlBxDispCl { disp });
            }
            let reg = Reg16::parse(r)
                .ok_or_else(|| AsmError::new(line.line_no, format!("shl: bad register `{r}`")))?;
            Ok(Instr::ShlReg16Cl { reg })
        }
        "sar" if rest.ends_with(",cl") => {
            let r = rest.strip_suffix(",cl").unwrap_or(rest);
            if let Some(reg) = Reg8::parse(r) {
                return Ok(Instr::SarReg8Cl { reg });
            }
            if r == "word ptr [si]" {
                return Ok(Instr::SarSiPtrCl);
            }
            if let Some((group, symbol)) = parse_byte_group_symbol(r) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::SarGroupSymByteCl {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            if let Some((group, symbol)) = parse_group_symbol(r) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::SarGroupSymCl {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `sar word ptr [bx+disp8], cl` — pointer-subscript
            // variable signed shift (sibling of `ShlBxDispCl`).
            if let Some(disp) = parse_word_bx_disp(r)
                && disp != 0
            {
                return Ok(Instr::SarBxDispCl { disp });
            }
            let reg = Reg16::parse(r)
                .ok_or_else(|| AsmError::new(line.line_no, format!("sar: bad register `{r}`")))?;
            Ok(Instr::SarReg16Cl { reg })
        }
        "shr" if rest.ends_with(",cl") => {
            let r = rest.strip_suffix(",cl").unwrap_or(rest);
            if let Some(reg) = Reg8::parse(r) {
                return Ok(Instr::ShrReg8Cl { reg });
            }
            if r == "word ptr [si]" {
                return Ok(Instr::ShrSiPtrCl);
            }
            if let Some((group, symbol)) = parse_byte_group_symbol(r) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::ShrGroupSymByteCl {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            if let Some((group, symbol)) = parse_group_symbol(r) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::ShrGroupSymCl {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `shr word ptr [bx+disp8], cl` — pointer-subscript
            // variable unsigned shift sibling.
            if let Some(disp) = parse_word_bx_disp(r)
                && disp != 0
            {
                return Ok(Instr::ShrBxDispCl { disp });
            }
            let reg = Reg16::parse(r)
                .ok_or_else(|| AsmError::new(line.line_no, format!("shr: bad register `{r}`")))?;
            Ok(Instr::ShrReg16Cl { reg })
        }
        "shl" => parse_shl_one(rest, line.line_no),
        "rcl" => parse_rcl_one(rest, line.line_no),
        "sar" => parse_sar_one(rest, line.line_no),
        "shr" => parse_shr_one(rest, line.line_no),
        "rcr" => parse_rcr_one(rest, line.line_no),
        "inc" => {
            if let Some(reg) = Reg8::parse(rest) {
                return Ok(Instr::IncReg8 { reg });
            }
            if let Some(reg) = Reg16::parse(rest) {
                return Ok(Instr::IncReg16 { reg });
            }
            // `inc word ptr [bp+N]` — bp-relative stack-local
            // increment (fixture 547: `++a[1]` on int array).
            if let Some(offset) = parse_word_bp_relative(rest) {
                return Ok(Instr::IncBpRel { offset });
            }
            // `inc word ptr <group>:<sym>[bx]` — indexed-element
            // increment. Fixture 2949 (`arr[i] += 1`).
            if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(rest) {
                return Ok(Instr::IncGroupSymBxDisp {
                    group: group.to_string(),
                    symbol: symbol.to_string(),
                    disp,
                });
            }
            // `inc byte ptr <group>:<sym>[bx]` — byte sibling.
            // Fixture 3516 (`arr[i]++` for char global).
            if let Some((group, symbol, disp)) = parse_byte_group_symbol_bx_disp(rest) {
                return Ok(Instr::IncGroupSymBxDispByte {
                    group: group.to_string(),
                    symbol: symbol.to_string(),
                    disp,
                });
            }
            // `inc byte ptr <group>:<sym>[si]` / `[di]` — SI/DI
            // sibling. Used when the index local lives in SI or DI
            // directly. Fixture 3516.
            if let Some((group, symbol, disp)) = parse_byte_group_symbol_reg_disp(rest, "si") {
                return Ok(Instr::IncGroupSymSiDispByte {
                    group: group.to_string(),
                    symbol: symbol.to_string(),
                    disp,
                });
            }
            if let Some((group, symbol, disp)) = parse_byte_group_symbol_reg_disp(rest, "di") {
                return Ok(Instr::IncGroupSymDiDispByte {
                    group: group.to_string(),
                    symbol: symbol.to_string(),
                    disp,
                });
            }
            // `inc word ptr <group>:<sym>[+N]` — memory-direct
            // increment of a data-segment global. Fixture 512.
            if let Some((group, symbol)) = parse_group_symbol(rest) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::IncGroupSym {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `inc word ptr <sym>[+N]` — bare-symbol form used by
            // huge-model `g++` (no DGROUP prefix). Fixture 3864.
            if let Some(symbol) = rest.strip_prefix("word ptr ")
                && let Some(first) = symbol.chars().next()
                && (first == '_' || first == '@')
                && !symbol.contains(':')
                && !symbol.contains('[')
            {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::IncSym {
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `inc byte ptr <group>:<sym>[+N]` — byte sibling
            // (fixture 702: `g++;` discarded → memory-direct inc).
            if let Some((group, symbol)) = parse_byte_group_symbol(rest) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::IncGroupSymByte {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `inc byte ptr [si]` — char postinc through pointer
            // (fixture 714: `(*p)++` discarded).
            if rest == "byte ptr [si]" {
                return Ok(Instr::IncSiPtrByte);
            }
            // `inc byte ptr [si+disp]` / `[di+disp]` — char field/element postinc
            // through a reg-var pointer at a non-zero offset (`p->c++`).
            if let Some(disp) = parse_byte_si_disp(rest) {
                return Ok(Instr::IncDecByteRegDisp { disp: i16::from(disp), di: false, dec: false });
            }
            if let Some(disp) = parse_byte_di_disp(rest) {
                return Ok(Instr::IncDecByteRegDisp { disp: i16::from(disp), di: true, dec: false });
            }
            // `inc word ptr [si]` — int sibling (fixture 1290).
            if rest == "word ptr [si]" {
                return Ok(Instr::IncSiPtrWord);
            }
            // `inc byte ptr [bp+N]` — char-local-array postinc
            // (fixture 721).
            if let Some(offset) = parse_byte_bp_relative(rest) {
                return Ok(Instr::IncBpRelByte { offset });
            }
            // `inc word ptr [bx+disp8]` — pointer-subscript K=1
            // peephole (fixture 880).
            if let Some(disp) = parse_word_bx_disp(rest)
                && disp != 0
            {
                return Ok(Instr::IncBxDisp { disp });
            }
            // `inc byte ptr [bx+disp8]` — char-pointer K=1
            // peephole (fixture 886).
            if let Some(disp) = parse_byte_bx_disp(rest)
                && disp != 0
            {
                return Ok(Instr::IncBxDispByte { disp });
            }
            Err(AsmError::new(
                line.line_no,
                format!("inc: unsupported operand form `{rest}`"),
            ))
        }
        "dec" => {
            if let Some(reg) = Reg8::parse(rest) {
                return Ok(Instr::DecReg8 { reg });
            }
            if let Some(reg) = Reg16::parse(rest) {
                return Ok(Instr::DecReg16 { reg });
            }
            // `dec word ptr [bp+N]` — bp-relative stack-local
            // decrement.
            if let Some(offset) = parse_word_bp_relative(rest) {
                return Ok(Instr::DecBpRel { offset });
            }
            // `dec word ptr <group>:<sym>[bx]` — indexed-element
            // decrement.
            if let Some((group, symbol, disp)) = parse_group_symbol_bx_disp(rest) {
                return Ok(Instr::DecGroupSymBxDisp {
                    group: group.to_string(),
                    symbol: symbol.to_string(),
                    disp,
                });
            }
            // `dec byte ptr <group>:<sym>[si]` / `[di]` siblings.
            if let Some((group, symbol, disp)) = parse_byte_group_symbol_reg_disp(rest, "si") {
                return Ok(Instr::DecGroupSymSiDispByte {
                    group: group.to_string(),
                    symbol: symbol.to_string(),
                    disp,
                });
            }
            if let Some((group, symbol, disp)) = parse_byte_group_symbol_reg_disp(rest, "di") {
                return Ok(Instr::DecGroupSymDiDispByte {
                    group: group.to_string(),
                    symbol: symbol.to_string(),
                    disp,
                });
            }
            // `dec byte ptr <group>:<sym>[bx]` — byte sibling.
            if let Some((group, symbol, disp)) = parse_byte_group_symbol_bx_disp(rest) {
                return Ok(Instr::DecGroupSymBxDispByte {
                    group: group.to_string(),
                    symbol: symbol.to_string(),
                    disp,
                });
            }
            // `dec word ptr <group>:<sym>[+N]` — symmetric with inc.
            if let Some((group, symbol)) = parse_group_symbol(rest) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::DecGroupSym {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `dec word ptr <sym>[+N]` — bare-symbol form used by
            // huge-model `g--`. Sibling of `IncSym`.
            if let Some(symbol) = rest.strip_prefix("word ptr ")
                && let Some(first) = symbol.chars().next()
                && (first == '_' || first == '@')
                && !symbol.contains(':')
                && !symbol.contains('[')
            {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::DecSym {
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `dec byte ptr <group>:<sym>[+N]` — byte sibling.
            if let Some((group, symbol)) = parse_byte_group_symbol(rest) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::DecGroupSymByte {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            // `dec byte ptr [si]` — char postdec through pointer.
            if rest == "byte ptr [si]" {
                return Ok(Instr::DecSiPtrByte);
            }
            // `dec byte ptr [si+disp]` / `[di+disp]` — char field/element postdec.
            if let Some(disp) = parse_byte_si_disp(rest) {
                return Ok(Instr::IncDecByteRegDisp { disp: i16::from(disp), di: false, dec: true });
            }
            if let Some(disp) = parse_byte_di_disp(rest) {
                return Ok(Instr::IncDecByteRegDisp { disp: i16::from(disp), di: true, dec: true });
            }
            // `dec word ptr [si]` — int sibling.
            if rest == "word ptr [si]" {
                return Ok(Instr::DecSiPtrWord);
            }
            // `dec byte ptr [bp+N]` — char-local-array postdec.
            if let Some(offset) = parse_byte_bp_relative(rest) {
                return Ok(Instr::DecBpRelByte { offset });
            }
            // `dec word ptr [bx+disp8]` — sibling of `IncBxDisp`.
            if let Some(disp) = parse_word_bx_disp(rest)
                && disp != 0
            {
                return Ok(Instr::DecBxDisp { disp });
            }
            // `dec byte ptr [bx+disp8]` — char-pointer K=1
            // peephole sibling.
            if let Some(disp) = parse_byte_bx_disp(rest)
                && disp != 0
            {
                return Ok(Instr::DecBxDispByte { disp });
            }
            Err(AsmError::new(
                line.line_no,
                format!("dec: unsupported operand form `{rest}`"),
            ))
        }
        "je" | "jne" | "jl" | "jle" | "jg" | "jge" | "ja" | "jae" | "jb" | "jbe"
        | "jc" | "jnc" | "js" | "jns" | "jp" | "jnp" | "jo" | "jno" => {
            parse_jmp_cond(kw, rest, line.line_no)
        }
        "jmp" => parse_jmp(rest, line.line_no),
        "loop" => {
            let target = rest
                .strip_prefix("short")
                .map(str::trim_start)
                .unwrap_or(rest)
                .trim();
            if target.is_empty() {
                return Err(AsmError::new(line.line_no, "loop: missing target"));
            }
            Ok(Instr::LoopShort { target: target.to_string() })
        }
        "call" => {
            // `call near ptr <label>` — direct near call.
            if let Some(target) = rest
                .strip_prefix("near ptr ")
                .or_else(|| rest.strip_prefix("near\tptr "))
            {
                return Ok(Instr::CallNear(target.trim().to_string()));
            }
            // `call far ptr [bp+<offset>]` — indirect *far* call
            // through a 4-byte stack-resident fn-pointer. The
            // `far ptr` prefix is BCC's way of selecting the
            // `ff /3` ModRM extension over the near-call `ff /2`.
            // Must precede the `call far ptr <label>` direct-far
            // arm below — otherwise the `[bp-4]` operand would be
            // parsed as a label name and rejected as undeclared.
            // Fixture 2211.
            if let Some(inside) = rest
                .strip_prefix("far ptr ")
                .or_else(|| rest.strip_prefix("far\tptr "))
                && let Some(offset) = parse_bp_relative(inside)
            {
                return Ok(Instr::CallFarIndirectBpRel { offset });
            }
            // `call far ptr <label>` — direct far call to an
            // external function. Fixture 2210.
            if let Some(target) = rest
                .strip_prefix("far ptr ")
                .or_else(|| rest.strip_prefix("far\tptr "))
            {
                return Ok(Instr::CallFar(target.trim().to_string()));
            }
            // `call word ptr <group>:<sym>[bx]` — indirect through
            // an array of function pointers indexed by BX. Fixture
            // 2944.
            if let Some(inner) = rest.strip_prefix("word ptr ") {
                if let Some((before_bracket, _)) = inner.rsplit_once("[bx]") {
                    if let Some((group, symbol)) = before_bracket.split_once(':') {
                        let (sym, _off) = split_sym_offset(symbol);
                        return Ok(Instr::CallIndirectGroupSymBx {
                            group: group.to_string(),
                            symbol: sym.to_string(),
                        });
                    }
                }
            }
            // `call word ptr <group>:<sym>[+disp]` — indirect through
            // a global function-pointer, optionally at a non-zero
            // offset within an array. Fixtures 2607, 2209.
            if let Some((group, symbol)) =
                parse_group_symbol_with_width(rest, "word ptr ")
            {
                let (sym, off) = split_sym_offset(symbol);
                return Ok(Instr::CallIndirectGroupSym {
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    disp: off as u16,
                });
            }
            // `call word ptr [bp+<offset>]` — indirect through stack.
            if let Some(offset) = parse_bp_relative(rest) {
                return Ok(Instr::CallIndirectBpRel { offset });
            }
            // `call word ptr [bx]` — indirect through BX.
            if rest.trim() == "word ptr [bx]" {
                return Ok(Instr::CallIndirectBx);
            }
            Err(AsmError::new(
                line.line_no,
                format!("call: unsupported operand form `{rest}`"),
            ))
        }
        "ret" => {
            if rest.is_empty() {
                Ok(Instr::Ret)
            } else if let Some(imm) = parse_imm16(rest) {
                Ok(Instr::RetImm16 { imm: imm as u16 })
            } else {
                Err(AsmError::new(line.line_no, format!("ret: bad operand `{rest}`")))
            }
        }
        "leave" if rest.is_empty() => Ok(Instr::Leave),
        "enter" => {
            // `enter <stack>, <level>` — 186 prologue.
            let (s, l) = split_comma(rest).ok_or_else(|| {
                AsmError::new(line.line_no, format!("enter: expected `stack, level`, got {rest:?}"))
            })?;
            let stack = parse_imm16(s).ok_or_else(|| {
                AsmError::new(line.line_no, format!("enter: bad stack `{s}`"))
            })?;
            let level = parse_imm8(l).ok_or_else(|| {
                AsmError::new(line.line_no, format!("enter: bad level `{l}`"))
            })?;
            Ok(Instr::Enter { stack: stack as u16, level: level as u8 })
        }
        "retf" => {
            if rest.is_empty() {
                Ok(Instr::Retf)
            } else if let Some(imm) = parse_imm16(rest) {
                Ok(Instr::RetfImm16 { imm: imm as u16 })
            } else {
                Err(AsmError::new(line.line_no, format!("retf: bad operand `{rest}`")))
            }
        }
        "fld" => parse_fld(rest, line.line_no),
        "fstp" => parse_fstp(rest, line.line_no),
        "fld1" if rest.is_empty() => Ok(Instr::Fld1),
        "fldz" if rest.is_empty() => Ok(Instr::Fldz),
        "fchs" if rest.is_empty() => Ok(Instr::Fchs),
        "fcompp" if rest.is_empty() => Ok(Instr::Fcompp),
        "fild" => {
            if let Some(offset) = parse_word_bp_relative(rest) {
                return Ok(Instr::FildWordBpRel { offset });
            }
            Err(AsmError::new(
                line.line_no,
                format!("fild: unsupported operand form `{rest}`"),
            ))
        }
        "fadd" => parse_fpu_arith(rest, FpuArithOp::Add, "fadd", line.line_no),
        // `fsub` with no operand is the register-stack
        // `fsubp st(1),st0` shorthand BCC pairs with `fld1`.
        "fsub" if rest.is_empty() => Ok(Instr::FsubpStack),
        "fsub" => parse_fpu_arith(rest, FpuArithOp::Sub, "fsub", line.line_no),
        "fmul" => parse_fpu_arith(rest, FpuArithOp::Mul, "fmul", line.line_no),
        "fdiv" => parse_fpu_arith(rest, FpuArithOp::Div, "fdiv", line.line_no),
        "fcomp" => {
            if let Some(offset) = parse_dword_bp_relative(rest) {
                return Ok(Instr::FcompBpRel { width: FpuWidth::Dword, offset });
            }
            if let Some(offset) = parse_qword_bp_relative(rest) {
                return Ok(Instr::FcompBpRel { width: FpuWidth::Qword, offset });
            }
            if let Some((group, symbol)) = parse_group_symbol_with_width(rest, "dword ptr ") {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::FcompGroupSym {
                    width: FpuWidth::Dword,
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            if let Some((group, symbol)) = parse_group_symbol_with_width(rest, "qword ptr ") {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::FcompGroupSym {
                    width: FpuWidth::Qword,
                    group: group.to_string(),
                    symbol: sym.to_string(),
                    offset,
                });
            }
            Err(AsmError::new(line.line_no, format!("fcomp: unsupported operand form `{rest}`")))
        }
        "fstsw" => {
            if let Some(offset) = parse_word_bp_relative(rest) {
                return Ok(Instr::FstswWordBpRel { offset });
            }
            Err(AsmError::new(line.line_no, format!("fstsw: unsupported operand form `{rest}`")))
        }
        "fwait" if rest.is_empty() => Ok(Instr::Fwait),
        "sahf" if rest.is_empty() => Ok(Instr::Sahf),
        _ => Err(AsmError::new(
            line.line_no,
            format!("instruction `{kw}` not yet supported (operands {rest:?})"),
        )),
    }
}
