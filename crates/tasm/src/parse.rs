//! Build the [`Module`] IR from the lexed token stream. The BCC `-S`
//! dialect is highly regular, so this is a directive-driven walk rather
//! than a real grammar.

use crate::ir::{
    AsmError, AsmResult, Group, Instr, JmpCond, Module, Reg16, Reg8, SegAlign, SegCombine,
    SegItem, Segment,
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
        "push" => Reg16::parse(rest)
            .map(|reg| Instr::PushReg16 { reg })
            .ok_or_else(|| AsmError::new(line.line_no, format!("push: unsupported operand `{rest}`"))),
        "pop" => Reg16::parse(rest)
            .map(|reg| Instr::PopReg16 { reg })
            .ok_or_else(|| AsmError::new(line.line_no, format!("pop: unsupported operand `{rest}`"))),
        "mov" => parse_mov(rest, line.line_no),
        // Generic ALU forms `<op> ax,word ptr [bp+N]`. Some opcodes
        // also have special operand forms (`sub sp,imm`, `xor ax,ax`)
        // that take precedence — handled in the per-op parser below.
        "add" => parse_add(rest, line.line_no),
        "sub" => parse_sub(rest, line.line_no),
        "and" => parse_alu_ax_mem(rest, line.line_no, "and", |o| Instr::AndAxBpRel { offset: o }),
        "or" => parse_or(rest, line.line_no),
        "xor" => parse_xor(rest, line.line_no),
        "cmp" => parse_cmp(rest, line.line_no),
        "imul" => {
            // Two forms: `imul word ptr [bp+N]` (BpRel) and
            // `imul <reg16>` (single reg operand, fixture 155).
            if let Some(reg) = Reg16::parse(rest) {
                return Ok(Instr::ImulReg16 { reg });
            }
            parse_single_op_word_ptr(rest, line.line_no, "imul", |o| Instr::ImulBpRel { offset: o })
        }
        "idiv" => parse_single_op_word_ptr(rest, line.line_no, "idiv", |o| Instr::IdivBpRel { offset: o }),
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
        "shl" => parse_shl_one(rest, line.line_no),
        "inc" => {
            if let Some(reg) = Reg8::parse(rest) {
                return Ok(Instr::IncReg8 { reg });
            }
            if let Some(reg) = Reg16::parse(rest) {
                return Ok(Instr::IncReg16 { reg });
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
    if lhs == "ax" {
        if rhs == "word ptr [si]" {
            return Ok(Instr::MovAxFromSiPtr);
        }
        if rhs == "word ptr [bx]" {
            return Ok(Instr::MovAxFromBxPtr);
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
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            let (sym, offset) = split_sym_offset(symbol);
            return Ok(Instr::MovReg16WordGroupSym {
                reg,
                group: group.to_string(),
                symbol: sym.to_string(),
                offset,
            });
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
        // `mov word ptr [bp-N],ax` — store AX. Fixture 160 uses this
        // to stash the switch scrutinee into a stack slot before the
        // linear-search loop walks it.
        if rhs == "ax" {
            return Ok(Instr::MovBpRelAx { offset });
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
    }
    // LHS `word ptr [bx]` — store through BX pointer (fixture 144).
    if lhs == "word ptr [bx]" {
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovBxPtrImm { imm });
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
    if lhs == "sp" {
        let imm = parse_imm16(rhs)
            .ok_or_else(|| AsmError::new(line_no, format!("sub sp,?: bad imm `{rhs}`")))?;
        let imm_u8 = u8::try_from(imm)
            .map_err(|_| AsmError::new(line_no, format!("sub sp,{imm}: doesn't fit in u8")))?;
        return Ok(Instr::SubSpImm(imm_u8));
    }
    // Otherwise: try the AX/mem form.
    parse_alu_ax_mem(operands, line_no, "sub", |o| Instr::SubAxBpRel { offset: o })
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
    // `add word ptr [si],<imm8>` — read-modify-write through SI.
    // Fixture 182: `p->x += 5` where SI holds `p`.
    if lhs == "word ptr [si]" {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AddSiPtrImm8 { imm });
        }
    }
    // `add word ptr [bp+N],<imm8>` — read-modify-write on a stack
    // local. Fixture 184: `a[1] += 5` folds to bp-relative add.
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::AddBpRelImm8 { offset, imm });
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
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::CmpAxBpRel { offset });
        }
        // `cmp ax,K` — BCC uses the special AX-imm16 opcode (3D) for
        // every constant K, not the generic 83 F8 form.
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::CmpAxImm { imm });
        }
    }
    // `cmp word ptr [bp+N],imm8` — compare stack local to small imm.
    if let Some(offset) = parse_word_bp_relative(lhs) {
        if let Some(imm) = parse_imm8_signed(rhs) {
            return Ok(Instr::CmpBpRelImm8 { offset, imm });
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
    let reg = Reg16::parse(lhs)
        .ok_or_else(|| AsmError::new(line_no, format!("shl: bad register `{lhs}`")))?;
    Ok(Instr::ShlReg16One { reg })
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
/// where 83 /7 takes a sign-extended imm8.
fn parse_imm8_signed(s: &str) -> Option<i8> {
    let s = s.trim();
    let v = s.parse::<i32>().ok()?;
    i8::try_from(v).ok()
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
    // requiring the symbol to start with `_` or `@` (BCC's user-symbol
    // convention).
    if !sym.starts_with('_') && !sym.starts_with('@') {
        return None;
    }
    Some((group, sym))
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
