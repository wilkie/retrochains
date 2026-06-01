//! Build the [`Module`] IR from the lexed token stream. The BCC `-S`
//! dialect is highly regular, so this is a directive-driven walk rather
//! than a real grammar.

use crate::ir::{
    AsmError, AsmResult, FpuArithOp, FpuWidth, Group, Instr, JmpCond, Module, Reg16, Reg8,
    SegAlign, SegCombine, SegItem, Segment, SegReg,
};
use crate::lex::{tokenize, Line};

mod alu_arith;
mod alu_logic;
mod alu_mov;
mod fpu;
mod instr;
mod operands;
mod shifts;

pub(crate) use alu_arith::*;
pub(crate) use alu_logic::*;
pub(crate) use alu_mov::*;
pub(crate) use fpu::*;
pub(crate) use instr::*;
pub(crate) use operands::*;
pub(crate) use shifts::*;

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
            "dd" => self.parse_dd(&line)?,
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

    fn parse_dd(&mut self, line: &Line<'_>) -> AsmResult<()> {
        let seg = self.require_open_segment(line)?;
        let rest = line.rest.trim();
        // `dd <group>:<symbol>[+N]` — 4-byte far-pointer slot for
        // file-scope `char *p = "lit"` in compact / large / huge
        // models. Fixtures 3760 / 3761.
        if let Some((group, after)) = rest.split_once(':') {
            let (sym, extra_offset) = split_sym_offset(after.trim());
            self.module.segments[seg].items.push(SegItem::DdGroupSym {
                group: group.trim().to_string(),
                symbol: sym.to_string(),
                extra_offset,
            });
            return Ok(());
        }
        // `dd <symbol>[+N]` — bare-symbol form used by huge-model
        // file-scope far-pointer initializers. Fixture 3902.
        if let Some(first) = rest.chars().next()
            && (first == '_' || first == '@')
        {
            let (sym, extra_offset) = split_sym_offset(rest);
            self.module.segments[seg].items.push(SegItem::DdSym {
                symbol: sym.to_string(),
                extra_offset,
            });
            return Ok(());
        }
        Err(AsmError::new(
            line.line_no,
            format!("dd: unsupported form `{rest}`"),
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





















































pub(crate) enum BpWidth {
    Any,
    Byte,
    Word,
    Dword,
    Qword,
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
