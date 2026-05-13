//! Build the [`Module`] IR from the lexed token stream. The BCC `-S`
//! dialect is highly regular, so this is a directive-driven walk rather
//! than a real grammar.

use crate::ir::{
    AsmError, AsmResult, Group, Instr, Module, SegAlign, SegCombine, SegItem, Segment,
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
        "push" if rest == "bp" => Ok(Instr::PushBp),
        "push" if rest == "ax" => Ok(Instr::PushAx),
        "pop" if rest == "bp" => Ok(Instr::PopBp),
        "pop" if rest == "cx" => Ok(Instr::PopCx),
        "mov" => parse_mov(rest, line.line_no),
        "xor" if rest == "ax,ax" => Ok(Instr::XorAxAx),
        "sub" => parse_sub(rest, line.line_no),
        "dec" if rest == "sp" => Ok(Instr::DecSp),
        "add" => parse_add(rest, line.line_no),
        "jmp" => {
            let r = rest.strip_prefix("short").map(str::trim_start).unwrap_or(rest);
            Ok(Instr::JmpShort(r.to_string()))
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
    if lhs == "bp" && rhs == "sp" {
        return Ok(Instr::MovBpSp);
    }
    if lhs == "sp" && rhs == "bp" {
        return Ok(Instr::MovSpBp);
    }
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::MovAxBpRel { offset });
        }
        if let Some((group, symbol)) = parse_offset_group_symbol(rhs) {
            return Ok(Instr::MovAxOffsetGroupSym {
                group: group.to_string(),
                symbol: symbol.to_string(),
            });
        }
        if let Some((group, symbol)) = parse_group_symbol(rhs) {
            return Ok(Instr::MovAxGroupSym {
                group: group.to_string(),
                symbol: symbol.to_string(),
            });
        }
        if let Some(imm) = parse_imm16(rhs) {
            return Ok(Instr::MovAxImm(imm));
        }
    }
    if let Some(offset) = parse_bp_relative(lhs) {
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
    Err(AsmError::new(
        line_no,
        format!("sub: unsupported operand form `{operands}`"),
    ))
}

fn parse_add(operands: &str, line_no: usize) -> AsmResult<Instr> {
    let (lhs, rhs) = split_comma(operands).ok_or_else(|| {
        AsmError::new(line_no, format!("add: expected `lhs,rhs`, got {operands:?}"))
    })?;
    if lhs == "ax" {
        if let Some(offset) = parse_bp_relative(rhs) {
            return Ok(Instr::AddAxBpRel { offset });
        }
    }
    Err(AsmError::new(
        line_no,
        format!("add: unsupported operand form `{operands}`"),
    ))
}

fn split_comma(s: &str) -> Option<(&str, &str)> {
    s.find(',').map(|i| (s[..i].trim(), s[i + 1..].trim()))
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
/// Returns `(group, symbol)` as borrowed slices.
fn parse_group_symbol(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let inside = s
        .strip_prefix("word ptr ")
        .or_else(|| s.strip_prefix("byte ptr "))?;
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

/// Parse `word ptr [bp<sign><offset>]` or `[bp<sign><offset>]`.
/// Returns the signed displacement.
fn parse_bp_relative(s: &str) -> Option<i16> {
    let s = s.trim();
    let inside = s
        .strip_prefix("word ptr ")
        .or_else(|| s.strip_prefix("byte ptr "))
        .unwrap_or(s);
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
