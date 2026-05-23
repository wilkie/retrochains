//! Build the [`Module`] IR from the lexed token stream. The BCC `-S`
//! dialect is highly regular, so this is a directive-driven walk rather
//! than a real grammar.

use crate::ir::{
    AsmError, AsmResult, Group, Instr, JmpCond, Module, Reg16, Reg8, SegAlign, SegCombine,
    SegItem, Segment, SegReg,
};
use crate::lex::{tokenize, Line};

pub fn parse(source: &str) -> AsmResult<Module> {
    let lines = tokenize(source)?;
    let mut p = Parser {
        lines: lines.as_slice(),
        idx: 0,
        module: Module::default(),
        open_segment: None,
    };
    p.skip_preamble();
    p.parse_body()?;
    Ok(p.module)
}

struct Parser<'a> {
    lines: &'a [Line<'a>],
    idx: usize,
    module: Module,
    /// Index into `module.segments` for the currently open segment, or
    /// `None` between an `ends` and the next `segment`.
    open_segment: Option<usize>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Line<'a>> {
        self.lines.get(self.idx)
    }

    fn bump(&mut self) -> Option<&Line<'a>> {
        let r = self.lines.get(self.idx);
        self.idx += 1;
        r
    }

    /// Step past the BCC `-S` macro preamble. BCC's preamble is
    /// always wrapped in `ifndef ??version` / `endif`; everything
    /// inside (macro definitions, `else`-branch redefinitions) is
    /// skipped. Once we see the closing `endif` we're in real content.
    /// If the source has no preamble (depth never increments), we
    /// return immediately at the first non-preamble keyword.
    fn skip_preamble(&mut self) {
        let mut depth: u32 = 0;
        while let Some(line) = self.peek() {
            match line.keyword {
                "ifndef" | "if" | "ifdef" => {
                    depth += 1;
                    self.bump();
                }
                "endif" => {
                    self.bump();
                    if depth > 0 {
                        depth -= 1;
                    }
                    if depth == 0 {
                        return;
                    }
                }
                _ if depth > 0 => {
                    self.bump();
                }
                _ => return,
            }
        }
    }

    fn parse_body(&mut self) -> AsmResult<()> {
        while let Some(line) = self.peek() {
            // `end` ends the source.
            if line.keyword == "end" {
                self.bump();
                break;
            }
            self.dispatch_line()?;
        }
        Ok(())
    }

    fn dispatch_line(&mut self) -> AsmResult<()> {
        // SAFETY: caller checked peek() is Some, but borrow checker
        // wants the bump to release the borrow before we modify self.
        let line = *self.peek().expect("dispatch_line precondition");
        self.bump();

        match line.keyword {
            "?debug" => self.parse_debug(&line)?,
            "segment" => self.parse_segment_open(&line)?,
            "ends" => self.parse_segment_close(&line)?,
            "group" => self.parse_group(&line)?,
            "assume" => { /* ignore: assume directives don't affect OMF output */ }
            "label" => self.parse_anchor_label(&line)?,
            "proc" => self.parse_proc(&line)?,
            "endp" => {
                let seg = self.require_open_segment(&line)?;
                self.module.segments[seg].items.push(SegItem::EndProc);
            }
            "public" => {
                let name = line.rest.trim();
                if name.is_empty() {
                    return Err(AsmError::new(line.line_no, "public: missing name"));
                }
                self.module.publics.push(name.to_string());
            }
            "extrn" => self.parse_extrn(&line)?,
            "db" => self.parse_db(&line)?,
            "dw" => self.parse_dw(&line)?,
            // Empty keyword + label = lone `@1@50:` style label.
            "" => {
                if let Some(label) = line.label {
                    let seg = self.require_open_segment(&line)?;
                    self.module.segments[seg].items.push(SegItem::Label(label.to_string()));
                }
            }
            _ => self.parse_instruction(&line)?,
        }
        Ok(())
    }

    fn parse_debug(&mut self, line: &Line<'_>) -> AsmResult<()> {
        // Two forms: `?debug S "name"` and `?debug C HEXBYTES`.
        let rest = line.rest.trim();
        let mut it = rest.splitn(2, char::is_whitespace);
        let sub = it.next().unwrap_or("");
        let arg = it.next().unwrap_or("").trim();
        match sub {
            "S" => {
                let name = unquote(arg)
                    .ok_or_else(|| AsmError::new(line.line_no, "?debug S: expected quoted string"))?;
                self.module.source_name = name.to_string();
            }
            "C" => {
                let bytes = decode_hex(arg, line.line_no)?;
                self.module.debug_comments.push(bytes);
            }
            other => {
                return Err(AsmError::new(
                    line.line_no,
                    format!("?debug: unrecognized subdirective `{other}`"),
                ));
            }
        }
        Ok(())
    }

    fn parse_segment_open(&mut self, line: &Line<'_>) -> AsmResult<()> {
        let name = line
            .label
            .ok_or_else(|| AsmError::new(line.line_no, "segment: missing name"))?
            .to_string();
        // Find or create.
        let idx = self.module.segments.iter().position(|s| s.name == name);
        let idx = match idx {
            Some(i) => i,
            None => {
                let (align, combine, class) = parse_segment_attrs(line.rest, line.line_no)?;
                self.module.segments.push(Segment {
                    name,
                    align,
                    combine,
                    class,
                    items: Vec::new(),
                });
                self.module.segments.len() - 1
            }
        };
        self.open_segment = Some(idx);
        Ok(())
    }

    fn parse_segment_close(&mut self, _line: &Line<'_>) -> AsmResult<()> {
        self.open_segment = None;
        Ok(())
    }

    fn parse_group(&mut self, line: &Line<'_>) -> AsmResult<()> {
        let name = line
            .label
            .ok_or_else(|| AsmError::new(line.line_no, "group: missing name"))?
            .to_string();
        let segs: Vec<String> = line
            .rest
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        self.module.groups.push(Group { name, segments: segs });
        Ok(())
    }

    fn parse_anchor_label(&mut self, line: &Line<'_>) -> AsmResult<()> {
        // `<name>\tlabel\t<byte|word>` — the leading name comes through as
        // the line's label.
        let name = line
            .label
            .ok_or_else(|| AsmError::new(line.line_no, "label: missing name"))?;
        let seg = self.require_open_segment(line)?;
        self.module.segments[seg]
            .items
            .push(SegItem::Label(name.to_string()));
        Ok(())
    }

    fn parse_proc(&mut self, line: &Line<'_>) -> AsmResult<()> {
        let name = line
            .label
            .ok_or_else(|| AsmError::new(line.line_no, "proc: missing name"))?;
        let seg = self.require_open_segment(line)?;
        self.module.segments[seg]
            .items
            .push(SegItem::Proc(name.to_string()));
        // Also define a label at this offset, so jumps/calls resolve
        // the proc name as a code symbol.
        self.module.segments[seg]
            .items
            .push(SegItem::Label(name.to_string()));
        Ok(())
    }

    fn parse_db(&mut self, line: &Line<'_>) -> AsmResult<()> {
        let seg = self.require_open_segment(line)?;
        let rest = line.rest.trim();
        // Form 1: `db N dup (?)` — reserve N bytes of uninitialized space.
        if let Some(first) = rest.split_whitespace().next() {
            if let Ok(count) = first.parse::<u32>() {
                if rest[first.len()..].trim_start().starts_with("dup") {
                    self.module.segments[seg].items.push(SegItem::Pad(count));
                    return Ok(());
                }
            }
        }
        // Form 2: `db '<text>'` — quoted ASCII run. BCC emits runs of
        // printable ASCII this way (fixture 108: `db '%d'`). Embedded
        // non-printables get their own `db <decimal>` lines.
        if let Some(text) = rest.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            self.module.segments[seg]
                .items
                .push(SegItem::Db(text.as_bytes().to_vec()));
            return Ok(());
        }
        // Form 3: `db <decimal>` — single byte value.
        if let Ok(v) = rest.parse::<u32>() {
            let b = u8::try_from(v & 0xFF).expect("byte fits");
            self.module.segments[seg].items.push(SegItem::Db(vec![b]));
            return Ok(());
        }
        Err(AsmError::new(
            line.line_no,
            format!("db: unsupported form `{rest}`"),
        ))
    }

    fn parse_dw(&mut self, line: &Line<'_>) -> AsmResult<()> {
        let seg = self.require_open_segment(line)?;
        let rest = line.rest.trim();
        // `dw @<label>` — emit 2 bytes referencing the label's
        // segment-relative offset, with a FIXUPP. BCC uses this for
        // jump-table entries.
        if rest.starts_with('@') || rest.starts_with('_') {
            self.module.segments[seg]
                .items
                .push(SegItem::DwSym(rest.to_string()));
            return Ok(());
        }
        // `dw <group>:<symbol>[+N]` — DGROUP-framed slot. Fixture 192
        // (`char *p = "hi"` at file scope) drops `dw DGROUP:s@`.
        if let Some((group, after)) = rest.split_once(':') {
            let (sym, extra_offset) = split_sym_offset(after.trim());
            self.module.segments[seg].items.push(SegItem::DwGroupSym {
                group: group.trim().to_string(),
                symbol: sym.to_string(),
                extra_offset,
            });
            return Ok(());
        }
        // `dw <integer>` — 2 raw bytes (little-endian). No fixture
        // yet but easy to support.
        if let Ok(v) = rest.parse::<i32>() {
            if let Ok(v16) = i16::try_from(v).map(|x| x as u16).or_else(|_| u16::try_from(v)) {
                let bytes = v16.to_le_bytes().to_vec();
                self.module.segments[seg].items.push(SegItem::Db(bytes));
                return Ok(());
            }
        }
        Err(AsmError::new(
            line.line_no,
            format!("dw: unsupported form `{rest}`"),
        ))
    }

    fn parse_extrn(&mut self, line: &Line<'_>) -> AsmResult<()> {
        // `\textrn\t_name:near` — strip the `:near` type tag.
        let name = line
            .rest
            .split(':')
            .next()
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            return Err(AsmError::new(line.line_no, "extrn: missing name"));
        }
        self.module.externs.push(name.to_string());
        Ok(())
    }

    fn parse_instruction(&mut self, line: &Line<'_>) -> AsmResult<()> {
        let seg = self.require_open_segment(line)?;
        let instr = parse_instr(line)?;
        self.module.segments[seg].items.push(SegItem::Instr(instr));
        Ok(())
    }

    fn require_open_segment(&self, line: &Line<'_>) -> AsmResult<usize> {
        self.open_segment.ok_or_else(|| {
            AsmError::new(
                line.line_no,
                format!("`{}` outside any segment", line.keyword),
            )
        })
    }
}

// Local clone of Line so dispatch_line can copy out of the borrowed slice.
impl<'a> Copy for Line<'a> {}
impl<'a> Clone for Line<'a> {
    fn clone(&self) -> Self {
        *self
    }
}

fn parse_segment_attrs(rest: &str, line_no: usize) -> AsmResult<(SegAlign, SegCombine, String)> {
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

fn parse_instr(line: &Line<'_>) -> AsmResult<Instr> {
    let kw = line.keyword;
    let rest = line.rest.trim_end();
    match kw {
        "push" => {
            if rest == "ds" {
                return Ok(Instr::PushDs);
            }
            if rest == "ss" {
                return Ok(Instr::PushSs);
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
        "pop" => Reg16::parse(rest)
            .map(|reg| Instr::PopReg16 { reg })
            .ok_or_else(|| AsmError::new(line.line_no, format!("pop: unsupported operand `{rest}`"))),
        "mov" => parse_mov(rest, line.line_no),
        // Generic ALU forms `<op> ax,word ptr [bp+N]`. Some opcodes
        // also have special operand forms (`sub sp,imm`, `xor ax,ax`)
        // that take precedence — handled in the per-op parser below.
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
            // `test word ptr [bp+disp8], imm16` — local bit test
            // (fixture 1853).
            if let Some(offset) = parse_word_bp_relative(lhs)
                && let Some(imm) = parse_imm16(rhs)
            {
                return Ok(Instr::TestBpRelImm16 { offset, imm: imm as u16 });
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
            // `dec word ptr <group>:<sym>[+N]` — symmetric with inc.
            if let Some((group, symbol)) = parse_group_symbol(rest) {
                let (sym, offset) = split_sym_offset(symbol);
                return Ok(Instr::DecGroupSym {
                    group: group.to_string(),
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
        "je" | "jne" | "jl" | "jle" | "jg" | "jge" | "ja" | "jae" | "jb" | "jbe" => {
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
            // `call word ptr [bp+<offset>]` — indirect through stack.
            if let Some(offset) = parse_bp_relative(rest) {
                return Ok(Instr::CallIndirectBpRel { offset });
            }
            Err(AsmError::new(
                line.line_no,
                format!("call: unsupported operand form `{rest}`"),
            ))
        }
        "ret" => Ok(Instr::Ret),
        _ => Err(AsmError::new(
            line.line_no,
            format!("instruction `{kw}` not yet supported (operands {rest:?})"),
        )),
    }
}

fn parse_mov(operands: &str, line_no: usize) -> AsmResult<Instr> {
    // Forms we recognize:
    //   bp,sp                                  → MovBpSp
    //   sp,bp                                  → MovSpBp
    //   ax,<decimal>                           → MovAxImm
    //   ax,word ptr [bp<sign><offset>]         → MovAxBpRel
    //   word ptr [bp<sign><offset>],<decimal>  → MovBpRelImm
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("mov: expected `lhs,rhs`, got {operands:?}"))
    })?;
    // Generic 16-bit reg-to-reg move (`mov bp,sp`, `mov sp,bp`,
    // `mov ax,dx`, `mov si,ax`, etc.). Tried before per-register
    // dispatch so it catches every reg-to-reg pair uniformly.
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::MovReg16Reg16 { dst, src });
    }
    // `mov <reg16>, <segreg>` — segment-register to general-purpose
    // register copy (`mov dx, ds`, `mov dx, ss`). Used to form the
    // segment half of a far pointer before calling helpers that
    // take far pointers in DX:AX (e.g. `N_SPUSH@`). Fixture 420.
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), SegReg::parse(rhs)) {
        return Ok(Instr::MovReg16SegReg { dst, src });
    }
    // `mov bx,word ptr [bx]` — chain step for `**p` (fixture 195).
    if lhs == "bx" && rhs == "word ptr [bx]" {
        return Ok(Instr::MovBxFromBxPtr);
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
        // `mov al,byte ptr [bx+disp8]` — char-pointer subscript
        // load (fixture 865). disp=0 stays with `MovAlFromBxPtr`
        // (2-byte form).
        if let Some(disp) = parse_byte_bx_disp(rhs)
            && disp != 0
        {
            return Ok(Instr::MovAlBxDisp { disp });
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
    // LHS `word ptr [si+disp]` — store-imm to long pointer's high
    // half (fixture 308: `*p = K` where `p: long *` in SI emits
    // `mov word ptr [si+2], <high>` after the low-half partner).
    if let Some(disp) = parse_word_si_disp(lhs) {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovSiDispImm { disp, imm });
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

fn parse_sub(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("sub: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::SubReg16Reg16 { dst, src });
    }
    if lhs == "sp" {
        let imm = parse_imm16(rhs)
            .ok_or_else(|| AsmError::new(line_no, format!("sub sp,?: bad imm `{rhs}`")))?;
        let imm_u8 = u8::try_from(imm)
            .map_err(|_| AsmError::new(line_no, format!("sub sp,{imm}: doesn't fit in u8")))?;
        return Ok(Instr::SubSpImm(imm_u8));
    }
    // `sub ax,word ptr [si]` — deref through SI as RHS (fixture 201).
    if lhs == "ax" && rhs == "word ptr [si]" {
        return Ok(Instr::SubAxFromSiPtr);
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
    // Otherwise: try the AX/mem form.
    parse_alu_ax_mem(operands, line_no, "sub", |o| Instr::SubAxBpRel { offset: o })
}

/// `and` covers `and ax,word ptr [bp+N]` (existing) plus the
/// long-arithmetic group-sym forms `and {ax|dx},word ptr <group>:<sym>[+N]`
/// (fixture 221).
fn parse_and(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
    // `and word ptr [si], ax` — int `*p &= y` through SI.
    if lhs == "word ptr [si]" && rhs == "ax" {
        return Ok(Instr::AndSiPtrAx);
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

/// `sbb ax, word ptr <group>:<sym>[+N]` — subtract-with-borrow,
/// long-arithmetic high-half (fixture 220).
fn parse_sbb(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
fn parse_adc(operands: &str, line_no: usize) -> AsmResult<Instr> {
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

/// Generic `<op> word ptr [bp+<offset>]` parser — single-operand
/// instructions like `imul`, `idiv`, `mul`, `div`, `neg`, `not`.
fn parse_single_op_word_ptr(
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

/// `add <lhs>,<rhs>` covers `add ax,word ptr [bp+N]` (fixture 113),
/// `add <reg16>,<reg16>` (fixture 127: `add ax,si`), and
/// `add ax,word ptr DGROUP:<sym>` (fixture 131: globals).
fn parse_add(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("add: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if let (Some(dst), Some(src)) = (Reg16::parse(lhs), Reg16::parse(rhs)) {
        return Ok(Instr::AddReg16Reg16 { dst, src });
    }
    if lhs == "sp" {
        let imm = parse_imm16(rhs)
            .ok_or_else(|| AsmError::new(line_no, format!("add sp,?: bad imm `{rhs}`")))?;
        let imm_u8 = u8::try_from(imm)
            .map_err(|_| AsmError::new(line_no, format!("add sp,{imm}: doesn't fit in u8")))?;
        return Ok(Instr::AddSpImm(imm_u8));
    }
    if lhs == "ax" {
        if rhs == "word ptr [si]" {
            return Ok(Instr::AddAxFromSiPtr);
        }
        if rhs == "word ptr [di]" {
            return Ok(Instr::AddAxFromDiPtr);
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
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::AddAxImm { imm });
        }
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
        // `add <reg16>, word ptr [bp+N]` — generic register-vs-stack
        // compound `+=` on a non-AX reg local (fixture 661). AX uses
        // its dedicated `AddAxBpRel` variant above.
        if !matches!(reg, Reg16::Ax) {
            if let Some(offset) = parse_bp_relative(rhs) {
                return Ok(Instr::AddReg16BpRel { reg, offset });
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
fn parse_cmp(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("cmp: expected `lhs,rhs`, got {operands:?}"))
    })?;
    // `cmp al, byte ptr [bp+N]` — char-vs-char compare peephole.
    // Fixture 951.
    if lhs == "al" {
        if let Some(offset) = parse_byte_bp_relative(rhs) {
            return Ok(Instr::CmpAlBpRel { offset });
        }
    }
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::CmpAxBpRel { offset });
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
    }
    if let Some(reg) = Reg8::parse(lhs) {
        if let Some(imm) = parse_imm8(rhs) {
            return Ok(Instr::CmpReg8Imm8 { reg, imm });
        }
    }
    Err(AsmError::new(
        line_no,
        format!("cmp: unsupported operand form `{operands}`"),
    ))
}

/// `jmp` covers two forms BCC emits in -ms: `jmp short <label>`
/// (intra-segment near jump, 2 bytes) and `jmp word ptr cs:<sym>[bx]`
/// (jump-table dispatch, 5 bytes + FIXUPP — fixture 158).
fn parse_jmp(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
fn parse_shl_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("shl: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if rhs != "1" {
        return Err(AsmError::new(
            line_no,
            format!("shl: only `<reg>,1` and `ax,cl` forms supported (got `{rhs}`)"),
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
fn parse_rcl_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
fn parse_sar_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
fn parse_shr_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
fn parse_rcr_one(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
fn parse_lea(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("lea: expected `lhs,rhs`, got {operands:?}"))
    })?;
    let dst = Reg16::parse(lhs)
        .ok_or_else(|| AsmError::new(line_no, format!("lea: bad dst `{lhs}`")))?;
    let offset = parse_bp_relative(rhs)
        .ok_or_else(|| AsmError::new(line_no, format!("lea: unsupported source `{rhs}`")))?;
    Ok(Instr::LeaReg16BpRel { dst, offset })
}

/// `or <lhs>,<rhs>` covers `or <reg16>,<reg16>` (fixture 132's
/// `or ax,ax` zero-test idiom) and `or ax,word ptr [bp+N]`.
fn parse_or(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
    // `or word ptr [si], ax` — int `*p |= y` through SI.
    if lhs == "word ptr [si]" && rhs == "ax" {
        return Ok(Instr::OrSiPtrAx);
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
fn parse_xor(operands: &str, line_no: usize) -> AsmResult<Instr> {
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
    // `xor word ptr [si], ax` — int `*p ^= y` through SI.
    if lhs == "word ptr [si]" && rhs == "ax" {
        return Ok(Instr::XorSiPtrAx);
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

/// Generic `<op> ax,word ptr [bp+<offset>]` parser. The four ALU
/// opcodes (add/sub/and/or/xor/cmp) share the same operand shape;
/// only the resulting IR variant differs.
fn parse_alu_ax_mem(
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

fn parse_jmp_cond(kw: &str, operands: &str, line_no: usize) -> AsmResult<Instr> {
    let cond = match kw {
        "je" => JmpCond::E,
        "jne" => JmpCond::Ne,
        "jl" => JmpCond::L,
        "jle" => JmpCond::Le,
        "jg" => JmpCond::G,
        "jge" => JmpCond::Ge,
        "ja" => JmpCond::A,
        "jae" => JmpCond::Ae,
        "jb" => JmpCond::B,
        "jbe" => JmpCond::Be,
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

fn split_comma(s: &str) -> Option<(&str, &str)> {
    s.find(',').map(|i| (s[..i].trim(), s[i + 1..].trim()))
}

/// Parse a decimal literal as an 8-bit value. None if the string
/// isn't a bare decimal in `-128..=255`.
fn parse_imm8(s: &str) -> Option<u8> {
    let s = s.trim();
    let v = s.parse::<i32>().ok()?;
    if (-128..=255).contains(&v) {
        Some(v as u8)
    } else {
        None
    }
}

/// Parse a decimal literal as a signed 8-bit value (sign-extends to
/// i16 at the instruction's caller). Used for `cmp <reg16>,<imm>`
/// where 83 /7 takes a sign-extended imm8. We also accept u16 values
/// in the upper half (32768..65535) by reinterpreting them as the
/// equivalent i16 — codegen frequently passes negative constants as
/// their unsigned 16-bit bit pattern (e.g. -5 = 65531). Fixture 563.
fn parse_imm8_signed(s: &str) -> Option<i8> {
    let s = s.trim();
    let v = s.parse::<i32>().ok()?;
    if let Ok(b) = i8::try_from(v) {
        return Some(b);
    }
    if (32_768..=65_535).contains(&v) {
        let as_i16 = v as i16; // reinterpret bit pattern
        return i8::try_from(as_i16).ok();
    }
    None
}

/// Parse a decimal literal as a 16-bit value. Returns `None` if the
/// string isn't a bare decimal. (BCC always uses bare decimals in
/// operands — no hex `42h` or octal forms.)
fn parse_imm16(s: &str) -> Option<u16> {
    let s = s.trim();
    if let Ok(v) = s.parse::<i32>() {
        if (-32_768..=65_535).contains(&v) {
            return Some(v as u16);
        }
    }
    None
}

/// Parse `offset <symbol>` (no group prefix) — e.g. `offset _f`.
/// The frame for such fixups is the target's own segment (F5).
fn parse_offset_symbol(s: &str) -> Option<&str> {
    let s = s.trim();
    let sym = s.strip_prefix("offset ")?;
    let sym = sym.trim();
    // Reject `offset GROUP:sym` forms here — those route through
    // parse_offset_group_symbol instead.
    if sym.contains(':') {
        return None;
    }
    if sym.is_empty() {
        return None;
    }
    Some(sym)
}

/// Parse `offset <group>:<symbol>` (e.g. `offset DGROUP:s@`).
fn parse_offset_group_symbol(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let inside = s.strip_prefix("offset ")?;
    let (group, sym) = inside.split_once(':')?;
    let group = group.trim();
    let sym = sym.trim();
    if group.is_empty() || sym.is_empty() {
        return None;
    }
    Some((group, sym))
}

/// Parse `word ptr <group>:<symbol>` (e.g. `word ptr DGROUP:_x`).
/// Returns `(group, symbol)`.
fn parse_group_symbol(s: &str) -> Option<(&str, &str)> {
    parse_group_symbol_with_width(s, "word ptr ")
}

/// Same, but requires `byte ptr` (`byte ptr DGROUP:_g`).
fn parse_byte_group_symbol(s: &str) -> Option<(&str, &str)> {
    parse_group_symbol_with_width(s, "byte ptr ")
}

fn parse_group_symbol_with_width<'a>(s: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let s = s.trim();
    let inside = s.strip_prefix(prefix)?;
    let (group, sym) = inside.split_once(':')?;
    let group = group.trim();
    let sym = sym.trim();
    if group.is_empty() || sym.is_empty() {
        return None;
    }
    // Discriminate against `cs:_TEXT` style addressing-prefix uses by
    // requiring the symbol to look like a BCC-emitted symbol: start
    // with `_`/`@`, or be one of BCC's reserved aggregate-pool labels
    // (`s@` for the constant string/blob pool, `d@` for the data
    // pool). Without the explicit allowlist, `mov ax, word ptr
    // DGROUP:s@` would fail to parse and stack-init reads (1612,
    // 1613) wouldn't assemble.
    let leading = sym.chars().next();
    let is_pool_label = sym == "s@" || sym == "d@" || sym.starts_with("s@") || sym.starts_with("d@");
    if !matches!(leading, Some('_') | Some('@')) && !is_pool_label {
        return None;
    }
    Some((group, sym))
}

/// Parse `word ptr [si+K]` or `word ptr [si-K]` (also accepts `[si]`,
/// returning disp=0). Returns the signed displacement.
fn parse_word_si_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("word ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "si" {
        return Some(0);
    }
    let rest = inside.strip_prefix("si")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}

/// Parse `word ptr [bx]` or `word ptr [bx+K]`/`word ptr [bx-K]` —
/// BX-based addressing with optional disp8. Returns the (signed)
/// displacement (0 if absent). Used by the global-pointer compound
/// path `p[K] += y` where BCC loads the pointer into BX and emits
/// `<op> word ptr [bx+offset], ax` (fixture 862).
fn parse_word_bx_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("word ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "bx" {
        return Some(0);
    }
    let rest = inside.strip_prefix("bx")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}

/// Byte-width sibling of [`parse_word_bx_disp`]. Recognizes
/// `byte ptr [bx]` and `byte ptr [bx+K]`/`byte ptr [bx-K]` used
/// by char-pointer subscripts (`char *p; p[K] op= …`, fixture 865).
fn parse_byte_bx_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("byte ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "bx" {
        return Some(0);
    }
    let rest = inside.strip_prefix("bx")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}

/// Parse `word ptr <group>:<sym>[bx]` or `word ptr <group>:<sym>[bx+K]`,
/// returning `(group, sym, disp)`. The displacement defaults to 0 when
/// `[bx]` has no `+K`. Used by variable-indexed long-array reads
/// (fixture 303: `mov ax, word ptr DGROUP:_a[bx+2]`).
fn parse_group_symbol_bx_disp(s: &str) -> Option<(&str, &str, u16)> {
    let s = s.trim().strip_prefix("word ptr ")?;
    let (group, rest) = s.split_once(':')?;
    let group = group.trim();
    // rest is `_sym[bx]` or `_sym[bx+K]`.
    let (sym_part, idx_part) = rest.split_once('[')?;
    let sym = sym_part.trim();
    if !sym.starts_with('_') && !sym.starts_with('@') {
        return None;
    }
    let idx = idx_part.strip_suffix(']')?.trim();
    let disp = if idx == "bx" {
        0u16
    } else if let Some(k) = idx.strip_prefix("bx+") {
        k.trim().parse::<u16>().ok()?
    } else {
        return None;
    };
    Some((group, sym, disp))
}

/// Strip a trailing `+<integer>` from a symbol, returning
/// `(name, offset)`. `_a+2` → `("_a", 2)`. No `+` → `(s, 0)`.
fn split_sym_offset(s: &str) -> (&str, i16) {
    if let Some((name, off)) = s.split_once('+') {
        if let Ok(n) = off.trim().parse::<i16>() {
            return (name.trim(), n);
        }
    }
    (s, 0)
}

/// Parse `word ptr [bp<sign><offset>]` or `[bp<sign><offset>]`.
/// Returns the signed displacement.
fn parse_bp_relative(s: &str) -> Option<i16> {
    parse_bp_relative_with_width(s, BpWidth::Any)
}

/// Same as [`parse_bp_relative`] but requires an explicit `byte ptr`
/// prefix. Used when an 8-bit operand context shouldn't accidentally
/// accept a `word ptr` reference.
fn parse_byte_bp_relative(s: &str) -> Option<i16> {
    parse_bp_relative_with_width(s, BpWidth::Byte)
}

/// Parse `byte ptr [si]` or `byte ptr [si+K]`/`byte ptr [si-K]` —
/// SI-based byte addressing with optional disp8. Returns the (signed)
/// displacement. Used by the char-pointer subscript byte-store path
/// (fixture 1016: `p[K] = 'X'` with p in SI).
fn parse_byte_si_disp(s: &str) -> Option<i8> {
    let s = s.trim().strip_prefix("byte ptr ")?;
    let inside = s.strip_prefix('[')?.strip_suffix(']')?;
    if inside == "si" {
        return Some(0);
    }
    let rest = inside.strip_prefix("si")?;
    let signed: i32 = rest.parse().ok()?;
    i8::try_from(signed).ok()
}

/// Same as [`parse_bp_relative`] but requires an explicit `word ptr`
/// prefix. Used on LHS stack-store opcodes where the width prefix
/// chooses the opcode (C6 vs C7).
fn parse_word_bp_relative(s: &str) -> Option<i16> {
    parse_bp_relative_with_width(s, BpWidth::Word)
}

enum BpWidth {
    Any,
    Byte,
    Word,
}

fn parse_bp_relative_with_width(s: &str, width: BpWidth) -> Option<i16> {
    let s = s.trim();
    let inside = match width {
        BpWidth::Any => s
            .strip_prefix("word ptr ")
            .or_else(|| s.strip_prefix("byte ptr "))
            .unwrap_or(s),
        BpWidth::Byte => s.strip_prefix("byte ptr ")?,
        BpWidth::Word => s.strip_prefix("word ptr ")?,
    };
    let inside = inside.strip_prefix('[')?.strip_suffix(']')?;
    let inside = inside.strip_prefix("bp")?;
    let inside = inside.trim_start();
    if inside.is_empty() {
        return Some(0);
    }
    inside.parse::<i16>().ok()
}

fn unquote(s: &str) -> Option<&str> {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

fn decode_hex(s: &str, line_no: usize) -> AsmResult<Vec<u8>> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.len() % 2 != 0 {
        return Err(AsmError::new(
            line_no,
            format!("?debug C: hex blob has odd length: {trimmed:?}"),
        ));
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let b = trimmed.as_bytes();
    for chunk in b.chunks_exact(2) {
        let hi = hex_digit(chunk[0], line_no)?;
        let lo = hex_digit(chunk[1], line_no)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(c: u8, line_no: usize) -> AsmResult<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        other => Err(AsmError::new(
            line_no,
            format!("invalid hex digit: {:?}", char::from(other)),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_001_ASM: &str = include_str!("../../../fixtures/001-empty-main/expected/HELLO.ASM");

    #[test]
    fn parses_fixture_001_skeleton() {
        let m = parse(FIXTURE_001_ASM).expect("parse");
        assert_eq!(m.source_name, "hello.c");
        // Two ?debug C records: the timestamped open at the top, the
        // bare `E9` close at the bottom.
        assert_eq!(m.debug_comments.len(), 2);
        assert_eq!(m.debug_comments[0][0], 0xE9);
        assert_eq!(m.debug_comments[1], vec![0xE9]);
        // Three segments: _TEXT, _DATA, _BSS (declaration order).
        let names: Vec<_> = m.segments.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["_TEXT", "_DATA", "_BSS"]);
        // One group, DGROUP = _DATA, _BSS.
        assert_eq!(m.groups.len(), 1);
        assert_eq!(m.groups[0].name, "DGROUP");
        assert_eq!(m.groups[0].segments, vec!["_DATA", "_BSS"]);
        // One public: _main.
        assert_eq!(m.publics, vec!["_main"]);
        // _TEXT body: proc _main, label _main, instructions..., label @1@50, ...
        let text = &m.segments[0];
        let labels: Vec<&String> = text
            .items
            .iter()
            .filter_map(|it| match it {
                SegItem::Label(n) => Some(n),
                _ => None,
            })
            .collect();
        assert!(labels.iter().any(|l| *l == "_main"));
        assert!(labels.iter().any(|l| *l == "@1@50"));
        let instr_count = text.items.iter().filter(|it| matches!(it, SegItem::Instr(_))).count();
        // push bp, mov bp,sp, xor ax,ax, jmp short @1@50, pop bp, ret
        assert_eq!(instr_count, 6);
    }
}
