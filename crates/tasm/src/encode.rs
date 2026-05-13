//! Encode parsed instructions into machine bytes. Each segment is
//! encoded independently after a module-wide pre-pass has resolved
//! every label to `(segment-index, offset-within-segment)`.

use std::collections::HashMap;

use crate::ir::{
    AsmError, AsmResult, FixupKind, FixupReq, Instr, Module, SegItem, Segment,
};

/// One segment's encoded output. The notional `size` (used for the
/// SEGDEF length field) can exceed `bytes.len()` when the segment
/// contains uninitialized padding (`db N dup (?)` items).
pub struct EncodedSeg {
    pub size: u32,
    pub bytes: Vec<u8>,
    pub fixups: Vec<FixupReq>,
}

/// Symbol table built across the entire module before encoding any
/// segment. Maps each defined label/anchor to the segment it lives
/// in (0-based index into `module.segments`) and its offset within
/// that segment.
pub type Symbols = HashMap<String, SymbolLoc>;

#[derive(Debug, Clone, Copy)]
pub struct SymbolLoc {
    pub segment: usize,
    pub offset: u16,
}

pub struct EncodedModule {
    pub segments: Vec<EncodedSeg>,
    pub symbols: Symbols,
}

pub fn encode_module(module: &Module) -> AsmResult<EncodedModule> {
    let symbols = build_symbols(module)?;
    let group_idx = build_group_idx(module);
    let segment_idx = build_segment_idx(module);
    let extern_idx = build_extern_idx(module);

    let mut segments = Vec::with_capacity(module.segments.len());
    for (i, seg) in module.segments.iter().enumerate() {
        segments.push(encode_segment(
            i,
            seg,
            &symbols,
            &group_idx,
            &segment_idx,
            &extern_idx,
        )?);
    }
    let _ = (segment_idx, group_idx, extern_idx); // consumed by encode_segment
    Ok(EncodedModule { segments, symbols })
}

fn build_extern_idx(module: &Module) -> HashMap<String, u8> {
    module
        .externs
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), u8::try_from(i + 1).expect("extdef idx fits")))
        .collect()
}

fn build_symbols(module: &Module) -> AsmResult<Symbols> {
    let mut out: Symbols = HashMap::new();
    for (seg_idx, seg) in module.segments.iter().enumerate() {
        let mut pc: u32 = 0;
        for item in &seg.items {
            match item {
                SegItem::Label(name) => {
                    let offset = u16::try_from(pc).map_err(|_| {
                        AsmError::new(0, format!("segment exceeds 64K at label `{name}`"))
                    })?;
                    out.insert(
                        name.clone(),
                        SymbolLoc {
                            segment: seg_idx,
                            offset,
                        },
                    );
                }
                SegItem::Proc(_) | SegItem::EndProc => {}
                SegItem::Db(b) => pc += b.len() as u32,
                SegItem::Pad(n) => pc += *n,
                SegItem::Instr(instr) => pc += instr_size(instr) as u32,
            }
        }
    }
    Ok(out)
}

fn build_group_idx(module: &Module) -> HashMap<String, u8> {
    module
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.name.clone(), u8::try_from(i + 1).expect("group idx fits")))
        .collect()
}

fn build_segment_idx(module: &Module) -> HashMap<String, u8> {
    module
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.clone(), u8::try_from(i + 1).expect("seg idx fits")))
        .collect()
}

fn encode_segment(
    seg_idx: usize,
    seg: &Segment,
    symbols: &Symbols,
    group_idx: &HashMap<String, u8>,
    _segment_idx: &HashMap<String, u8>,
    extern_idx: &HashMap<String, u8>,
) -> AsmResult<EncodedSeg> {
    // Walk items, but distinguish between "in the LEDATA byte stream"
    // and "still part of this segment, just padding". Items can't
    // freely interleave today; in practice each segment is either all
    // padding (`_BSS`) or all bytes/instructions (`_TEXT`, `_DATA`).
    // We track them separately and refuse interleaving (asserts).

    let mut bytes: Vec<u8> = Vec::new();
    let mut fixups: Vec<FixupReq> = Vec::new();
    let mut pad: u32 = 0;
    let mut sealed_bytes = false; // once we've started padding, no more bytes
    let mut sealed_pad = false; // once we've emitted bytes, no more padding

    for item in &seg.items {
        match item {
            SegItem::Label(_) | SegItem::Proc(_) | SegItem::EndProc => {}
            SegItem::Db(b) => {
                if sealed_bytes {
                    return Err(AsmError::new(
                        0,
                        format!("segment {}: `db` after padding not supported", seg.name),
                    ));
                }
                sealed_pad = true;
                bytes.extend_from_slice(b);
            }
            SegItem::Pad(n) => {
                if sealed_pad {
                    return Err(AsmError::new(
                        0,
                        format!("segment {}: padding after concrete bytes not supported", seg.name),
                    ));
                }
                sealed_bytes = true;
                pad += *n;
            }
            SegItem::Instr(instr) => {
                if sealed_bytes {
                    return Err(AsmError::new(
                        0,
                        format!("segment {}: instruction after padding not supported", seg.name),
                    ));
                }
                sealed_pad = true;
                emit_instr(
                    seg_idx,
                    instr,
                    symbols,
                    group_idx,
                    extern_idx,
                    &mut bytes,
                    &mut fixups,
                )?;
            }
        }
    }

    let size = u32::try_from(bytes.len()).unwrap() + pad;
    Ok(EncodedSeg { size, bytes, fixups })
}

fn instr_size(instr: &Instr) -> usize {
    match instr {
        Instr::PushBp
        | Instr::PopBp
        | Instr::PushAx
        | Instr::PopCx
        | Instr::Ret
        | Instr::DecSp => 1,
        Instr::MovBpSp | Instr::MovSpBp | Instr::XorAxAx | Instr::JmpShort(_) => 2,
        Instr::JmpCondShort { .. } => 2,
        Instr::MovAxImm(_) | Instr::SubSpImm(_) => 3,
        Instr::MovAxBpRel { .. }
        | Instr::AddAxBpRel { .. }
        | Instr::SubAxBpRel { .. }
        | Instr::AndAxBpRel { .. }
        | Instr::OrAxBpRel { .. }
        | Instr::XorAxBpRel { .. }
        | Instr::CmpAxBpRel { .. } => 3,
        Instr::CallNear(_) => 3,
        Instr::MovAxGroupSym { .. } | Instr::MovAxOffsetGroupSym { .. } => 3,
        Instr::MovBpRelImm { .. } | Instr::MovBpRelOffsetSym { .. } => 5,
        Instr::CallIndirectBpRel { .. } => 3,
    }
}

fn emit_instr(
    seg_idx: usize,
    instr: &Instr,
    symbols: &Symbols,
    group_idx: &HashMap<String, u8>,
    extern_idx: &HashMap<String, u8>,
    out: &mut Vec<u8>,
    fixups: &mut Vec<FixupReq>,
) -> AsmResult<()> {
    match instr {
        Instr::PushBp => out.push(0x55),
        Instr::PushAx => out.push(0x50),
        Instr::PopBp => out.push(0x5D),
        Instr::PopCx => out.push(0x59),
        Instr::MovBpSp => {
            out.push(0x8B);
            out.push(0xEC);
        }
        Instr::MovSpBp => {
            out.push(0x8B);
            out.push(0xE5);
        }
        Instr::XorAxAx => {
            out.push(0x33);
            out.push(0xC0);
        }
        Instr::MovAxImm(imm) => {
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::SubSpImm(imm) => {
            out.push(0x83);
            out.push(0xEC);
            out.push(*imm);
        }
        Instr::DecSp => out.push(0x4C),
        Instr::MovBpRelImm { offset, imm } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0xC7);
            out.push(0x46);
            out.push(disp as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovAxBpRel { offset } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x8B);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::AddAxBpRel { offset } => emit_alu_ax_bp_rel(0x03, *offset, out),
        Instr::SubAxBpRel { offset } => emit_alu_ax_bp_rel(0x2B, *offset, out),
        Instr::AndAxBpRel { offset } => emit_alu_ax_bp_rel(0x23, *offset, out),
        Instr::OrAxBpRel { offset } => emit_alu_ax_bp_rel(0x0B, *offset, out),
        Instr::XorAxBpRel { offset } => emit_alu_ax_bp_rel(0x33, *offset, out),
        Instr::CmpAxBpRel { offset } => emit_alu_ax_bp_rel(0x3B, *offset, out),
        Instr::JmpCondShort { cond, target } => {
            let target_off = symbols.get(target).map(|l| l.offset).ok_or_else(|| {
                AsmError::new(0, format!("Jcc: unresolved label `{target}`"))
            })?;
            let here = out.len() + 2;
            let disp = i32::from(target_off) - here as i32;
            let rel8 = i8::try_from(disp).map_err(|_| {
                AsmError::new(
                    0,
                    format!("Jcc displacement {disp} out of i8 range to `{target}`"),
                )
            })?;
            out.push(cond.opcode_byte());
            out.push(rel8 as u8);
        }
        Instr::CallNear(target) => {
            // E8 lo hi. Resolve target's segment-relative offset.
            // - If it's a label in *this* segment: compute rel16 at
            //   compile time. No FIXUPP.
            // - If it's an extern or cross-segment label: emit zeros
            //   and queue an extern FIXUPP. (Extern handling lands
            //   with fixture 108.)
            match symbols.get(target) {
                Some(loc) if loc.segment == seg_idx => {
                    let here = out.len() + 3;
                    let disp = i32::from(loc.offset) - here as i32;
                    let rel16 = i16::try_from(disp).map_err(|_| {
                        AsmError::new(
                            0,
                            format!("call near rel16 out of range to `{target}`"),
                        )
                    })?;
                    out.push(0xE8);
                    out.extend_from_slice(&rel16.to_le_bytes());
                }
                _ => {
                    // Extern call: emit `E8 00 00` and queue a
                    // self-relative FIXUPP to the EXTDEF entry.
                    let idx = *extern_idx.get(target).ok_or_else(|| {
                        AsmError::new(
                            0,
                            format!("call near: `{target}` not in this TU and not declared extern"),
                        )
                    })?;
                    out.push(0xE8);
                    let imm_start = out.len();
                    out.push(0x00);
                    out.push(0x00);
                    fixups.push(FixupReq {
                        data_offset: u16::try_from(imm_start).expect("offset fits"),
                        kind: FixupKind::SelfRelExtern { extdef_idx: idx },
                    });
                }
            }
        }
        Instr::MovAxGroupSym { group, symbol } => {
            // `mov ax,word ptr <group>:<symbol>` → A1 lo hi.
            // Encoding A1 is `mov AX, moffs16` — segment-relative load.
            emit_group_sym_lea(0xA1, group, symbol, symbols, group_idx, out, fixups)?;
        }
        Instr::MovAxOffsetGroupSym { group, symbol } => {
            // `mov ax,offset <group>:<symbol>` → B8 lo hi (mov ax,imm16
            // where imm16 = symbol's segment-relative offset). Same
            // FIXUPP shape as MovAxGroupSym.
            emit_group_sym_lea(0xB8, group, symbol, symbols, group_idx, out, fixups)?;
        }
        Instr::MovBpRelOffsetSym { offset, symbol } => {
            // `mov word ptr [bp+disp8],offset _f` → C7 46 dd lo hi.
            // The imm bytes carry the symbol's segment-relative
            // offset (which TLINK will patch via the FIXUPP). The
            // FIXUPP frame is F5 (target's own segment) because the
            // target is in _TEXT, which is not in any group.
            let sym_loc = symbols.get(symbol).ok_or_else(|| {
                AsmError::new(0, format!("symbol `{symbol}` not defined in any segment"))
            })?;
            let target_seg_idx =
                u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0xC7);
            out.push(0x46);
            out.push(disp as u8);
            let imm_start = out.len();
            out.extend_from_slice(&sym_loc.offset.to_le_bytes());
            fixups.push(FixupReq {
                data_offset: u16::try_from(imm_start).expect("offset fits"),
                kind: FixupKind::SegRelTargetFrameSegment {
                    segment_idx: target_seg_idx,
                },
            });
        }
        Instr::CallIndirectBpRel { offset } => {
            // `call word ptr [bp+disp8]` → FF 56 dd. ModR/M 56 =
            // mod=01 /2(call near r/m16) r/m=110 ([bp+disp8]).
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0xFF);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::JmpShort(target) => {
            let target_off = symbols.get(target).map(|l| l.offset).ok_or_else(|| {
                AsmError::new(0, format!("jmp short: unresolved label `{target}`"))
            })?;
            let here = out.len() + 2;
            let disp = i32::from(target_off) - here as i32;
            let rel8 = i8::try_from(disp).map_err(|_| {
                AsmError::new(
                    0,
                    format!("jmp short displacement {disp} out of i8 range to `{target}`"),
                )
            })?;
            out.push(0xEB);
            out.push(rel8 as u8);
        }
        Instr::Ret => out.push(0xC3),
    }
    Ok(())
}

/// Encode an `<op> ax,word ptr [bp+disp]` instruction. The opcode
/// byte varies (03=add, 2B=sub, 23=and, 0B=or, 33=xor, 3B=cmp); the
/// ModR/M byte is always 0x46 (mod=01, reg=000=AX, r/m=110=[bp+disp8]).
fn emit_alu_ax_bp_rel(opcode: u8, offset: i16, out: &mut Vec<u8>) {
    let disp = i8::try_from(offset).expect("bp-relative offset fits in i8");
    out.push(opcode);
    out.push(0x46);
    out.push(disp as u8);
}

/// Shared helper for the two `mov ax,<form>:<sym>` instructions
/// (A1-encoded load and B8-encoded offset-immediate). Both emit the
/// same SegRelGroupTarget FIXUPP — the only difference is the opcode.
fn emit_group_sym_lea(
    opcode: u8,
    group: &str,
    symbol: &str,
    symbols: &Symbols,
    group_idx: &HashMap<String, u8>,
    out: &mut Vec<u8>,
    fixups: &mut Vec<FixupReq>,
) -> AsmResult<()> {
    let sym_loc = symbols.get(symbol).ok_or_else(|| {
        AsmError::new(0, format!("symbol `{symbol}` not defined in any segment"))
    })?;
    let g_idx = *group_idx
        .get(group)
        .ok_or_else(|| AsmError::new(0, format!("group `{group}` not defined")))?;
    let target_seg_idx = u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
    out.push(opcode);
    let imm_start = out.len();
    out.extend_from_slice(&sym_loc.offset.to_le_bytes());
    fixups.push(FixupReq {
        data_offset: u16::try_from(imm_start).expect("offset fits"),
        kind: FixupKind::SegRelGroupTarget {
            group_idx: g_idx,
            segment_idx: target_seg_idx,
        },
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{SegAlign, SegCombine};

    fn make_seg(name: &str) -> Segment {
        Segment {
            name: name.into(),
            align: SegAlign::Byte,
            combine: SegCombine::Public,
            class: "CODE".into(),
            items: Vec::new(),
        }
    }

    #[test]
    fn fixture_002_main_body() {
        let mut seg = make_seg("_TEXT");
        seg.items = vec![
            SegItem::Instr(Instr::PushBp),
            SegItem::Instr(Instr::MovBpSp),
            SegItem::Instr(Instr::XorAxAx),
            SegItem::Instr(Instr::JmpShort("@1@50".into())),
            SegItem::Label("@1@50".into()),
            SegItem::Instr(Instr::PopBp),
            SegItem::Instr(Instr::Ret),
        ];
        let module = Module {
            segments: vec![seg],
            ..Module::default()
        };
        let em = encode_module(&module).unwrap();
        assert_eq!(em.segments[0].bytes, vec![0x55, 0x8B, 0xEC, 0x33, 0xC0, 0xEB, 0x00, 0x5D, 0xC3]);
        assert_eq!(em.segments[0].size, 9);
        assert!(em.segments[0].fixups.is_empty());
    }
}
