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
                SegItem::DwSym(_) | SegItem::DwGroupSym { .. } => pc += 2,
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
            SegItem::DwSym(name) => {
                if sealed_bytes {
                    return Err(AsmError::new(
                        0,
                        format!("segment {}: `dw` after padding not supported", seg.name),
                    ));
                }
                sealed_pad = true;
                let sym_loc = symbols.get(name).ok_or_else(|| {
                    AsmError::new(0, format!("dw: symbol `{name}` not defined"))
                })?;
                let imm_start = bytes.len();
                bytes.extend_from_slice(&sym_loc.offset.to_le_bytes());
                let target_seg_idx =
                    u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
                fixups.push(FixupReq {
                    data_offset: u16::try_from(imm_start).expect("offset fits"),
                    kind: FixupKind::SegRelTargetFrameSegment {
                        segment_idx: target_seg_idx,
                    },
                });
            }
            SegItem::DwGroupSym { group, symbol, extra_offset } => {
                if sealed_bytes {
                    return Err(AsmError::new(
                        0,
                        format!("segment {}: `dw` after padding not supported", seg.name),
                    ));
                }
                sealed_pad = true;
                let sym_loc = symbols.get(symbol).ok_or_else(|| {
                    AsmError::new(0, format!("dw: symbol `{symbol}` not defined"))
                })?;
                let g_idx = *group_idx.get(group).ok_or_else(|| {
                    AsmError::new(0, format!("dw: group `{group}` not defined"))
                })?;
                let target_seg_idx =
                    u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
                let value = sym_loc.offset.wrapping_add(*extra_offset as u16);
                let imm_start = bytes.len();
                bytes.extend_from_slice(&value.to_le_bytes());
                fixups.push(FixupReq {
                    data_offset: u16::try_from(imm_start).expect("offset fits"),
                    kind: FixupKind::SegRelGroupTarget {
                        group_idx: g_idx,
                        segment_idx: target_seg_idx,
                    },
                });
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
        Instr::Ret => 1,
        Instr::PushReg16 { .. }
        | Instr::PopReg16 { .. }
        | Instr::IncReg16 { .. }
        | Instr::DecReg16 { .. } => 1,
        Instr::MovReg16Reg16 { .. }
        | Instr::XorReg16Reg16 { .. }
        | Instr::AddReg16Reg16 { .. }
        | Instr::AdcReg16Reg16 { .. }
        | Instr::SubReg16Reg16 { .. }
        | Instr::SbbReg16Reg16 { .. }
        | Instr::AndReg16Reg16 { .. }
        | Instr::OrReg16Reg16 { .. }
        | Instr::CmpReg16Reg16 { .. } => 2,
        Instr::CmpReg16Imm8 { .. } | Instr::CmpAxImm { .. } | Instr::AddAxImm { .. } => 3,
        Instr::CmpBpRelImm8 { .. } => 4,
        Instr::JmpShort(_) | Instr::ShlAxCl | Instr::SarAxCl | Instr::ShrAxCl => 2,
        Instr::Cwd => 1,
        Instr::JmpCondShort { .. } => 2,
        Instr::JmpIndirectCsTableBx { .. } => 5,
        Instr::JmpIndirectCsBxDisp { .. } => 4,
        Instr::LoopShort { .. } => 2,
        Instr::MovBpRelAx { .. } | Instr::MovBpRelReg16 { .. } => 3,
        Instr::MovAxFromCsBx => 3,
        Instr::MovReg16OffsetSym { .. } => 3,
        Instr::MovReg16GroupSymBxDisp { .. } => 4,
        Instr::MovGroupSymBxDispImm { .. } => 6,
        Instr::MovReg16Imm { .. } | Instr::SubSpImm(_) | Instr::AddSpImm(_) => 3,
        Instr::MovReg16BpRel { .. }
        | Instr::AddAxBpRel { .. }
        | Instr::AdcDxBpRel { .. }
        | Instr::SbbDxBpRel { .. }
        | Instr::AddDxBpRel { .. }
        | Instr::AdcAxBpRel { .. }
        | Instr::SubDxBpRel { .. }
        | Instr::SbbAxBpRel { .. }
        | Instr::AndDxBpRel { .. }
        | Instr::OrDxBpRel { .. }
        | Instr::XorDxBpRel { .. }
        | Instr::AddBpRelDx { .. }
        | Instr::AdcBpRelAx { .. }
        | Instr::SubBpRelDx { .. }
        | Instr::SbbBpRelAx { .. }
        | Instr::AndBpRelDx { .. }
        | Instr::AndBpRelAx { .. }
        | Instr::OrBpRelDx { .. }
        | Instr::OrBpRelAx { .. }
        | Instr::XorBpRelDx { .. }
        | Instr::XorBpRelAx { .. }
        | Instr::SubAxBpRel { .. }
        | Instr::AndAxBpRel { .. }
        | Instr::OrAxBpRel { .. }
        | Instr::XorAxBpRel { .. }
        | Instr::CmpAxBpRel { .. }
        | Instr::CmpDxBpRel { .. }
        | Instr::ImulBpRel { .. }
        | Instr::IdivBpRel { .. }
        | Instr::MovReg8BpRel { .. }
        | Instr::MovBpRelReg8 { .. } => 3,
        Instr::MovReg8Imm8 { .. } => 2,
        Instr::MovReg8Reg8 { .. } => 2,
        Instr::MovBpRelImm8 { .. } => 4,
        Instr::IncReg8 { .. } | Instr::DecReg8 { .. } => 2,
        Instr::CmpReg8Imm8 { .. } => 3,
        Instr::CallNear(_) => 3,
        Instr::MovAxGroupSym { .. }
        | Instr::MovAlGroupSym { .. }
        | Instr::MovReg16OffsetGroupSym { .. } => 3,
        Instr::MovReg16WordGroupSym { .. } => 4,
        Instr::MovGroupSymImm16 { .. } => 6,
        Instr::MovGroupSymImm8 { .. } => 5,
        Instr::MovGroupSymAx { .. } => 3,
        Instr::MovGroupSymReg16 { .. } => 4,
        Instr::AddReg16Imm8Sx { .. }
        | Instr::AdcReg16Imm8Sx { .. }
        | Instr::SbbReg16Imm8Sx { .. } => 3,
        Instr::AddReg16Imm16 { .. } => 4,
        Instr::AddGroupSymImm16 { .. } => 6,
        Instr::AdcAxImm16 { .. } | Instr::SbbAxImm16 { .. } => 3,
        Instr::MovAlFromSiPtr | Instr::MovAlFromBxPtr => 2,
        Instr::ImulReg16 { .. } => 2,
        Instr::AddAxGroupSym { .. }
        | Instr::OrAxGroupSym { .. }
        | Instr::AddDxGroupSym { .. }
        | Instr::AdcAxGroupSym { .. }
        | Instr::AddGroupSymDx { .. }
        | Instr::AdcGroupSymAx { .. }
        | Instr::AdcDxGroupSym { .. }
        | Instr::SubDxGroupSym { .. }
        | Instr::SbbAxGroupSym { .. }
        | Instr::AndDxGroupSym { .. }
        | Instr::AndAxGroupSym { .. }
        | Instr::OrDxGroupSym { .. }
        | Instr::XorDxGroupSym { .. }
        | Instr::XorAxGroupSym { .. } => 4,
        Instr::CmpAxGroupSym { .. } | Instr::CmpDxGroupSym { .. } => 4,
        Instr::PushGroupSym { .. } => 4,
        Instr::PushBpRel { .. } | Instr::PushSiDisp { .. } => 3,
        Instr::PushSiPtr => 2,
        Instr::PushDs => 1,
        Instr::PushSs => 1,
        Instr::MovReg16SegReg { .. } => 2,
        Instr::CmpGroupSymImm8Sx { .. }
        | Instr::CmpByteGroupSymImm8 { .. }
        | Instr::AddGroupSymImm8Sx { .. }
        | Instr::AdcGroupSymImm8Sx { .. }
        | Instr::SubGroupSymImm8Sx { .. }
        | Instr::SbbGroupSymImm8Sx { .. } => 5,
        Instr::AndGroupSymImm16 { .. }
        | Instr::OrGroupSymImm16 { .. }
        | Instr::XorGroupSymImm16 { .. }
        | Instr::CmpGroupSymImm16 { .. } => 6,
        Instr::Cbw => 1,
        Instr::LeaReg16BpRel { .. } => 3,
        Instr::MovSiPtrImm { .. } | Instr::MovBxPtrImm { .. } => 4,
        Instr::MovSiPtrImm8 { .. } => 3,
        Instr::MovSiDispImm { .. } => 5,
        Instr::MovAxSiDisp { .. } | Instr::MovDxSiDisp { .. } => 3,
        Instr::MovDxFromSiPtr => 2,
        Instr::AddSiPtrImm8 { .. } | Instr::AddBxPtrImm8 { .. } | Instr::SubSiPtrImm8 { .. } => 3,
        Instr::AdcSiDispImm8 { .. } | Instr::SbbSiDispImm8 { .. } => 4,
        Instr::AddSiPtrDx => 2,
        Instr::AdcSiDispAx { .. } => 3,
        Instr::AddBpRelImm8 { .. }
        | Instr::AdcBpRelImm8 { .. }
        | Instr::SubBpRelImm8 { .. }
        | Instr::SbbBpRelImm8 { .. } => 4,
        Instr::AndBpRelImm16 { .. }
        | Instr::OrBpRelImm16 { .. }
        | Instr::XorBpRelImm16 { .. } => 5,
        Instr::MovAxFromSiPtr
        | Instr::MovAxFromBxPtr
        | Instr::MovBxFromBxPtr
        | Instr::SubAxFromSiPtr
        | Instr::AddAxFromSiPtr => 2,
        Instr::ShlReg16One { .. }
        | Instr::RclReg16One { .. }
        | Instr::SarReg16One { .. }
        | Instr::ShrReg16One { .. }
        | Instr::RcrReg16One { .. }
        | Instr::NegReg16 { .. }
        | Instr::NotReg16 { .. } => 2,
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
        Instr::PushReg16 { reg } => out.push(0x50 | reg.code()),
        Instr::PopReg16 { reg } => out.push(0x58 | reg.code()),
        Instr::IncReg16 { reg } => out.push(0x40 | reg.code()),
        Instr::DecReg16 { reg } => out.push(0x48 | reg.code()),
        Instr::MovReg16Reg16 { dst, src } => {
            // `mov r16,r16` → 8B (mod=11 dst<<3 src). Same encoding
            // family as 8A for 8-bit registers.
            out.push(0x8B);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::XorReg16Reg16 { dst, src } => {
            // `xor r16,r16` → 33 (mod=11 dst<<3 src).
            out.push(0x33);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::AddReg16Reg16 { dst, src } => {
            // `add r16,r16` → 03 (mod=11 dst<<3 src).
            out.push(0x03);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::AdcReg16Reg16 { dst, src } => {
            // `adc r16,r16` → 13 (mod=11 dst<<3 src). Same shape as
            // `add r16,r16` but with carry propagation.
            out.push(0x13);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::SubReg16Reg16 { dst, src } => {
            // `sub r16,r16` → 2B (mod=11 dst<<3 src). Same ModR/M
            // shape as `add r16,r16`; opcode 2B selects SUB.
            out.push(0x2B);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::SbbReg16Reg16 { dst, src } => {
            // `sbb r16,r16` → 1B (mod=11 dst<<3 src). Borrow-
            // propagation high-half partner to `sub r16,r16`.
            out.push(0x1B);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::AndReg16Reg16 { dst, src } => {
            // `and r16,r16` → 23 (mod=11 dst<<3 src).
            out.push(0x23);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::OrReg16Reg16 { dst, src } => {
            // `or r16,r16` → 0B (mod=11 dst<<3 src).
            out.push(0x0B);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::CmpReg16Imm8 { reg, imm } => {
            // `cmp r16,imm8 (sign-extended)` → 83 (mod=11 /7 r/m=reg) ii.
            // 83 is Grp1 r/m16,imm8 sign-extended; /7 selects CMP.
            out.push(0x83);
            out.push(0b11_111_000 | reg.code());
            out.push(*imm as u8);
        }
        Instr::CmpReg16Reg16 { lhs, rhs } => {
            // `cmp r16,r/m16` → 3B (mod=11 lhs<<3 rhs). LHS goes in
            // the reg field, RHS in the r/m field.
            out.push(0x3B);
            out.push(0b11_000_000 | (lhs.code() << 3) | rhs.code());
        }
        Instr::CmpAxImm { imm } => {
            // `cmp ax,imm16` → 3D lo hi (AX-accumulator special form).
            out.push(0x3D);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::AddAxImm { imm } => {
            // `add ax,imm16` → 05 lo hi.
            out.push(0x05);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::CmpBpRelImm8 { offset, imm } => {
            // `cmp word ptr [bp+disp8],imm8 (sign-extended)` → 83 7E dd ii.
            // ModR/M 7E = mod=01 /7(CMP) r/m=110([bp+disp8]).
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x83);
            out.push(0x7E);
            out.push(disp as u8);
            out.push(*imm as u8);
        }
        Instr::MovReg16Imm { reg, imm } => {
            // `mov r16,imm16` → B8+rc lo hi.
            out.push(0xB8 | reg.code());
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::SubSpImm(imm) => {
            // `sub sp,imm8` → 83 EC ii (Grp1 r/m16,imm8 sign-extended;
            // ModR/M EC = mod=11 /5(SUB) r/m=SP).
            out.push(0x83);
            out.push(0xEC);
            out.push(*imm);
        }
        Instr::AddSpImm(imm) => {
            // `add sp,imm8` → 83 C4 ii (ModR/M C4 = mod=11 /0(ADD)
            // r/m=SP). Used for cdecl stack cleanup after multi-arg
            // calls (fixture 138's `add sp,6`).
            out.push(0x83);
            out.push(0xC4);
            out.push(*imm);
        }
        Instr::MovBpRelImm { offset, imm } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0xC7);
            out.push(0x46);
            out.push(disp as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovReg16BpRel { reg, offset } => {
            // `mov r16,word ptr [bp+disp8]` → 8B xx dd. ModR/M xx =
            // mod=01 reg=<reg-code> r/m=110 ([bp+disp8]).
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x8B);
            out.push(0b01_000_110 | (reg.code() << 3));
            out.push(disp as u8);
        }
        Instr::AddAxBpRel { offset } => emit_alu_ax_bp_rel(0x03, *offset, out),
        Instr::SubAxBpRel { offset } => emit_alu_ax_bp_rel(0x2B, *offset, out),
        Instr::AdcDxBpRel { offset } => {
            // `adc dx,word ptr [bp+disp8]` → 13 56 dd. ModR/M 56 =
            // mod=01 reg=DX(010) rm=110.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x13);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::SbbDxBpRel { offset } => {
            // `sbb dx,word ptr [bp+disp8]` → 1B 56 dd. Same ModR/M
            // as AdcDxBpRel; opcode 1B is SBB r16,r/m16.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x1B);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::AddDxBpRel { offset } => {
            // `add dx,word ptr [bp+disp8]` → 03 56 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x03);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::AdcAxBpRel { offset } => {
            // `adc ax,word ptr [bp+disp8]` → 13 46 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x13);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::SubDxBpRel { offset } => {
            // `sub dx,word ptr [bp+disp8]` → 2B 56 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x2B);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::SbbAxBpRel { offset } => {
            // `sbb ax,word ptr [bp+disp8]` → 1B 46 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x1B);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::AndDxBpRel { offset } => {
            // `and dx,word ptr [bp+disp8]` → 23 56 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x23);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::OrDxBpRel { offset } => {
            // `or dx,word ptr [bp+disp8]` → 0B 56 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x0B);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::XorDxBpRel { offset } => {
            // `xor dx,word ptr [bp+disp8]` → 33 56 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x33);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::AddBpRelDx { offset } => {
            // `add word ptr [bp+disp8],dx` → 01 56 dd. Opcode 01 =
            // ADD r/m16,r16. ModR/M 56 = mod=01 reg=DX(010) r/m=110.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x01);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::AdcBpRelAx { offset } => {
            // `adc word ptr [bp+disp8],ax` → 11 46 dd. Opcode 11 =
            // ADC r/m16,r16. ModR/M 46 = mod=01 reg=AX(000) r/m=110.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x11);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::SubBpRelDx { offset } => {
            // `sub word ptr [bp+disp8],dx` → 29 56 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x29);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::SbbBpRelAx { offset } => {
            // `sbb word ptr [bp+disp8],ax` → 19 46 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x19);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::AndBpRelDx { offset } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x21);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::AndBpRelAx { offset } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x21);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::OrBpRelDx { offset } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x09);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::OrBpRelAx { offset } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x09);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::XorBpRelDx { offset } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x31);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::XorBpRelAx { offset } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x31);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::SubAxFromSiPtr => {
            // `sub ax,word ptr [si]` → 2B 04. 2B is `sub r16,r/m16`;
            // ModR/M 04 = mod=00 reg=AX r/m=100 ([si]).
            out.push(0x2B);
            out.push(0x04);
        }
        Instr::AddAxFromSiPtr => {
            // `add ax,word ptr [si]` → 03 04. Same ModR/M as the
            // `sub` sibling, opcode 03 (add r16,r/m16).
            out.push(0x03);
            out.push(0x04);
        }
        Instr::AndAxBpRel { offset } => emit_alu_ax_bp_rel(0x23, *offset, out),
        Instr::OrAxBpRel { offset } => emit_alu_ax_bp_rel(0x0B, *offset, out),
        Instr::XorAxBpRel { offset } => emit_alu_ax_bp_rel(0x33, *offset, out),
        Instr::CmpAxBpRel { offset } => emit_alu_ax_bp_rel(0x3B, *offset, out),
        Instr::CmpDxBpRel { offset } => {
            // `cmp dx,word ptr [bp+disp8]` → 3B 56 dd. ModR/M 56 =
            // mod=01 reg=DX(010) r/m=110([bp+disp8]).
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x3B);
            out.push(0x56);
            out.push(disp as u8);
        }
        Instr::ImulBpRel { offset } => {
            // `imul word ptr [bp+disp8]` → F7 6E dd. F7 is the Grp3
            // r/m16 escape; ModR/M 6E = mod=01 /5(IMUL) r/m=110.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0xF7);
            out.push(0x6E);
            out.push(disp as u8);
        }
        Instr::IdivBpRel { offset } => {
            // `idiv word ptr [bp+disp8]` → F7 7E dd. ModR/M 7E = /7(IDIV).
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0xF7);
            out.push(0x7E);
            out.push(disp as u8);
        }
        Instr::Cwd => out.push(0x99),
        Instr::MovReg8BpRel { reg, offset } => {
            // `mov <reg8>,byte ptr [bp+disp8]` → 8A xx dd. 8A = mov
            // r8,r/m8. ModR/M = mod=01 reg=<reg-code> r/m=110.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x8A);
            out.push(0b01_000_110 | (reg.code() << 3));
            out.push(disp as u8);
        }
        Instr::MovBpRelReg8 { offset, reg } => {
            // `mov byte ptr [bp+disp8],<reg8>` → 88 xx dd. 88 = mov
            // r/m8,r8 (note source/dest swap vs 8A). ModR/M = mod=01
            // reg=<reg-code> r/m=110.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x88);
            out.push(0b01_000_110 | (reg.code() << 3));
            out.push(disp as u8);
        }
        Instr::MovReg8Imm8 { reg, imm } => {
            // `mov <reg8>,imm8` → B0+rc ii.
            out.push(0xB0 | reg.code());
            out.push(*imm);
        }
        Instr::MovReg8Reg8 { dst, src } => {
            // `mov <dst>,<src>` (both r8) → 8A xx where xx is mod=11,
            // reg=<dst-code>, r/m=<src-code>.
            out.push(0x8A);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::MovBpRelImm8 { offset, imm } => {
            // `mov byte ptr [bp+disp8],imm8` → C6 46 dd ii.
            // C6 = mov r/m8,imm8. ModR/M 46 = mod=01 /0 r/m=110.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0xC6);
            out.push(0x46);
            out.push(disp as u8);
            out.push(*imm);
        }
        Instr::IncReg8 { reg } => {
            // `inc <reg8>` → FE C0+rc. FE = Grp4 r/m8. ModR/M mod=11
            // /0 r/m=<reg-code>.
            out.push(0xFE);
            out.push(0xC0 | reg.code());
        }
        Instr::DecReg8 { reg } => {
            // `dec <reg8>` → FE C8+rc. ModR/M mod=11 /1 r/m=<reg-code>.
            out.push(0xFE);
            out.push(0xC8 | reg.code());
        }
        Instr::CmpReg8Imm8 { reg, imm } => {
            // `cmp <reg8>,imm8` → 80 F8+rc ii. 80 = Grp1 r/m8,imm8.
            // ModR/M mod=11 /7(CMP) r/m=<reg-code>.
            out.push(0x80);
            out.push(0xF8 | reg.code());
            out.push(*imm);
        }
        Instr::ShlAxCl => {
            // `shl ax,cl` → D3 E0. D3 = Grp2 r/m16,CL. ModR/M E0 =
            // mod=11 /4(SHL) r/m=000(AX).
            out.push(0xD3);
            out.push(0xE0);
        }
        Instr::SarAxCl => {
            // `sar ax,cl` → D3 F8. ModR/M F8 = mod=11 /7(SAR) r/m=AX.
            out.push(0xD3);
            out.push(0xF8);
        }
        Instr::ShrAxCl => {
            // `shr ax,cl` → D3 E8. ModR/M E8 = mod=11 /5(SHR) r/m=AX.
            out.push(0xD3);
            out.push(0xE8);
        }
        Instr::JmpIndirectCsBxDisp { disp } => {
            // `jmp word ptr cs:[bx+disp8]` → 2E FF 67 dd.
            // 2E = CS override, FF /4 = JMP near r/m16, ModR/M 67 =
            // mod=01 reg=4(/4) r/m=111(BX) → [bx+disp8].
            out.push(0x2E);
            out.push(0xFF);
            out.push(0x67);
            out.push(*disp);
        }
        Instr::LoopShort { target } => {
            // `loop short <label>` → E2 rel8.
            let target_off = symbols.get(target).map(|l| l.offset).ok_or_else(|| {
                AsmError::new(0, format!("loop: unresolved label `{target}`"))
            })?;
            let here = out.len() + 2;
            let disp = i32::from(target_off) - here as i32;
            let rel8 = i8::try_from(disp).map_err(|_| {
                AsmError::new(0, format!("loop displacement {disp} out of i8 range"))
            })?;
            out.push(0xE2);
            out.push(rel8 as u8);
        }
        Instr::MovBpRelAx { offset } => {
            // `mov word ptr [bp+disp8],ax` → 89 46 dd. 89 is mov
            // r/m16,r16 (source/dest swap vs 8B); ModR/M 46 = mod=01
            // reg=AX r/m=110.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x89);
            out.push(0x46);
            out.push(disp as u8);
        }
        Instr::MovBpRelReg16 { offset, reg } => {
            // `mov word ptr [bp+disp8], r16` → 89 (mod=01 reg rm=110) dd.
            // Same opcode as `MovBpRelAx`; only the ModR/M reg field
            // changes.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x89);
            out.push(0b01_000_110 | (reg.code() << 3));
            out.push(disp as u8);
        }
        Instr::MovAxFromCsBx => {
            // `mov ax,word ptr cs:[bx]` → 2E 8B 07. ModR/M 07 =
            // mod=00 reg=AX r/m=111(BX).
            out.push(0x2E);
            out.push(0x8B);
            out.push(0x07);
        }
        Instr::MovReg16OffsetSym { reg, symbol } => {
            // `mov r16,offset <code-symbol>` → (B8+rc) lo hi.
            // FIXUPP frame = target's segment (F5), no group.
            let sym_loc = symbols.get(symbol).ok_or_else(|| {
                AsmError::new(0, format!("symbol `{symbol}` not defined"))
            })?;
            out.push(0xB8 | reg.code());
            let imm_start = out.len();
            out.extend_from_slice(&sym_loc.offset.to_le_bytes());
            let target_seg_idx =
                u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
            fixups.push(FixupReq {
                data_offset: u16::try_from(imm_start).expect("offset fits"),
                kind: FixupKind::SegRelTargetFrameSegment {
                    segment_idx: target_seg_idx,
                },
            });
        }
        Instr::JmpIndirectCsTableBx { table } => {
            // `jmp word ptr cs:<table>[bx]` → 2E FF A7 lo hi.
            // 2E = CS segment override prefix.
            // FF = Grp5 r/m16.
            // A7 = mod=10 /4(JMP near r/m16) r/m=111(BX, mod=10 →
            //      [bx+disp16]).
            // lo hi = the table label's segment-relative offset.
            let sym_loc = symbols.get(table).ok_or_else(|| {
                AsmError::new(0, format!("jmp cs:<sym>[bx]: unresolved label `{table}`"))
            })?;
            out.push(0x2E);
            out.push(0xFF);
            out.push(0xA7);
            let imm_start = out.len();
            out.extend_from_slice(&sym_loc.offset.to_le_bytes());
            let target_seg_idx =
                u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
            fixups.push(FixupReq {
                data_offset: u16::try_from(imm_start).expect("offset fits"),
                kind: FixupKind::SegRelTargetFrameSegment {
                    segment_idx: target_seg_idx,
                },
            });
        }
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
        Instr::MovAxGroupSym { group, symbol, offset } => {
            // `mov ax,word ptr <group>:<symbol>` → A1 lo hi.
            // Encoding A1 is `mov AX, moffs16` — segment-relative load.
            emit_group_sym_lea(&[0xA1], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymImm16 { group, symbol, offset, imm } => {
            // `mov word ptr <group>:<sym>[+N], imm16` → C7 06 [addr]
            // [imm16]. Same FIXUPP shape as the `MovAxGroupSym` load
            // sibling, plus 2 trailing immediate bytes.
            emit_group_sym_lea(&[0xC7, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovGroupSymImm8 { group, symbol, offset, imm } => {
            // `mov byte ptr <group>:<sym>[+N], imm8` → C6 06 [addr]
            // [imm8]. Same FIXUPP shape but the byte opcode (C6 vs C7)
            // and a single trailing immediate byte.
            emit_group_sym_lea(&[0xC6, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm);
        }
        Instr::MovGroupSymAx { group, symbol, offset } => {
            // `mov word ptr <group>:<sym>[+N], ax` → A3 lo hi
            // (mov moffs16, AX) — the AX-specific store short form.
            emit_group_sym_lea(&[0xA3], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymReg16 { group, symbol, offset, reg } => {
            // `mov word ptr <group>:<sym>[+N], <reg16>` → 89 (mod=00
            // reg=<r> rm=110) lo hi. Non-AX dst takes the generic
            // `mov r/m16, r16` opcode.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x89, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AddReg16Imm8Sx { reg, imm } => {
            // `add <reg16>, imm8sx` → 83 C(reg) ii. ModR/M C(reg) =
            // mod=11 /0(ADD) rm=<reg>.
            out.push(0x83);
            out.push(0b11_000_000 | reg.code());
            out.push(*imm as u8);
        }
        Instr::AdcReg16Imm8Sx { reg, imm } => {
            // `adc <reg16>, imm8sx` → 83 D(reg) ii. ModR/M D(reg) =
            // mod=11 /2(ADC) rm=<reg>.
            out.push(0x83);
            out.push(0b11_010_000 | reg.code());
            out.push(*imm as u8);
        }
        Instr::SbbReg16Imm8Sx { reg, imm } => {
            // `sbb <reg16>, imm8sx` → 83 D(reg) ii. ModR/M D(reg) =
            // mod=11 /3(SBB) rm=<reg>.
            out.push(0x83);
            out.push(0b11_011_000 | reg.code());
            out.push(*imm as u8);
        }
        Instr::AddReg16Imm16 { reg, imm } => {
            // `add <reg16>, imm16` → 81 C(reg) lo hi. Same ModR/M
            // as the imm8sx form; opcode 81 selects the wider imm.
            out.push(0x81);
            out.push(0b11_000_000 | reg.code());
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::AddGroupSymImm16 { group, symbol, offset, imm } => {
            // `add word ptr <group>:<sym>[+N], imm16` → 81 06 lo hi imm_lo imm_hi.
            // Grp1 r/m16,imm16 with /0=ADD (fixture 276's `g += K` for big K).
            emit_group_sym_lea(&[0x81, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::AdcAxImm16 { imm } => {
            // `adc ax, imm16` → 15 lo hi.
            out.push(0x15);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::SbbAxImm16 { imm } => {
            // `sbb ax, imm16` → 1D lo hi. Companion to AdcAxImm16
            // for the high half of long unary minus (fixture 226).
            out.push(0x1D);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovReg16WordGroupSym { reg, group, symbol, offset } => {
            // `mov <reg16>,word ptr <group>:<sym>` → 8B (mod=00
            // reg=<r> rm=110) lo hi. Generic disp16-only addressing
            // for non-AX destinations; AX uses the shorter A1 form.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x8B, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovReg16GroupSymBxDisp { reg, group, symbol, disp } => {
            // `mov <reg16>,word ptr <group>:<sym>[bx+disp]` → 8B
            // (mod=10 reg=<r> r/m=111([bx]+disp16)) lo hi. The disp16
            // bytes are `<sym-offset> + <disp>`, FIXUPP-patched as the
            // symbol's segment-relative location.
            let modrm = 0b10_000_111 | (reg.code() << 3);
            emit_group_sym_lea(&[0x8B, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymBxDispImm { group, symbol, disp, imm } => {
            // `mov word ptr <group>:<sym>[bx+disp],imm16` → C7 87
            // lo hi imm_lo imm_hi.
            emit_group_sym_lea(&[0xC7, 0x87], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovAlGroupSym { group, symbol, offset } => {
            // `mov al,byte ptr <group>:<symbol>` → A0 lo hi.
            // A0 is the 8-bit moffs8 sibling of A1.
            emit_group_sym_lea(&[0xA0], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovReg16OffsetGroupSym { reg, group, symbol, offset } => {
            // `mov r16,offset <group>:<symbol>` → (B8+rc) lo hi.
            // Same FIXUPP shape as MovAxGroupSym. The single opcode
            // byte varies by destination register.
            let opcode = 0xB8 | reg.code();
            emit_group_sym_lea(&[opcode], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovAlFromSiPtr => {
            // `mov al,byte ptr [si]` → 8A 04. 8A is mov r8,r/m8.
            // ModR/M 04 = mod=00 reg=AL r/m=100([si]).
            out.push(0x8A);
            out.push(0x04);
        }
        Instr::MovAlFromBxPtr => {
            // `mov al,byte ptr [bx]` → 8A 07. Same opcode as the
            // SI form; ModR/M 07 = mod=00 reg=AL r/m=111([bx]).
            out.push(0x8A);
            out.push(0x07);
        }
        Instr::ImulReg16 { reg } => {
            // `imul r16` → F7 (mod=11 /5 r/m=<reg>).
            out.push(0xF7);
            out.push(0b11_101_000 | reg.code());
        }
        Instr::AddAxGroupSym { group, symbol, offset } => {
            // `add ax,word ptr <group>:<symbol>` → 03 06 lo hi.
            // ModR/M 06 = mod=00 reg=AX r/m=110 (disp16-only addressing).
            emit_group_sym_lea(&[0x03, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::OrAxGroupSym { group, symbol, offset } => {
            // `or ax,word ptr <group>:<symbol>` → 0B 06 lo hi.
            // Same ModR/M as the `add` sibling; opcode 0B (OR r16,r/m16).
            emit_group_sym_lea(&[0x0B, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AddDxGroupSym { group, symbol, offset } => {
            // `add dx,word ptr <group>:<symbol>` → 03 16 lo hi.
            // ModR/M 16 = mod=00 reg=DX(010) rm=110 (disp16-only).
            emit_group_sym_lea(&[0x03, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AdcAxGroupSym { group, symbol, offset } => {
            // `adc ax,word ptr <group>:<symbol>` → 13 06 lo hi.
            // Same ModR/M as the `add ax` sibling; opcode 13 (ADC r16,r/m16).
            emit_group_sym_lea(&[0x13, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AdcDxGroupSym { group, symbol, offset } => {
            // `adc dx,word ptr <group>:<symbol>` → 13 16 lo hi.
            // Opcode 13 (ADC r16,r/m16); ModR/M reg field 010=DX.
            emit_group_sym_lea(&[0x13, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AddGroupSymDx { group, symbol, offset } => {
            // `add word ptr <group>:<symbol>,dx` → 01 16 lo hi.
            // Opcode 01 (ADD r/m16,r16); ModR/M 16 = mod=00 reg=DX
            // rm=110 (disp16-only). Memory-dest sibling of
            // `AddDxGroupSym` (which goes the other way, reg dst).
            emit_group_sym_lea(&[0x01, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AdcGroupSymAx { group, symbol, offset } => {
            // `adc word ptr <group>:<symbol>,ax` → 11 06 lo hi.
            // Opcode 11 (ADC r/m16,r16); ModR/M 06 = mod=00 reg=AX
            // rm=110 (disp16-only). High-half partner to
            // `AddGroupSymDx` for memory-dest compound `+=`.
            emit_group_sym_lea(&[0x11, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SubDxGroupSym { group, symbol, offset } => {
            // `sub dx,word ptr <group>:<symbol>` → 2B 16 lo hi.
            // Same shape as `AddDxGroupSym`; opcode 2B (SUB r16,r/m16).
            emit_group_sym_lea(&[0x2B, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SbbAxGroupSym { group, symbol, offset } => {
            // `sbb ax,word ptr <group>:<symbol>` → 1B 06 lo hi.
            // Companion to AdcAxGroupSym; opcode 1B (SBB r16,r/m16).
            emit_group_sym_lea(&[0x1B, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AndDxGroupSym { group, symbol, offset } => {
            // `and dx,word ptr <group>:<symbol>` → 23 16 lo hi.
            // Opcode 23 (AND r16,r/m16) with DX dst.
            emit_group_sym_lea(&[0x23, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AndAxGroupSym { group, symbol, offset } => {
            // `and ax,word ptr <group>:<symbol>` → 23 06 lo hi.
            // Same opcode as the DX form; ModR/M reg field 000=AX.
            emit_group_sym_lea(&[0x23, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::OrDxGroupSym { group, symbol, offset } => {
            // `or dx,word ptr <group>:<symbol>` → 0B 16 lo hi.
            // Opcode 0B (OR r16,r/m16); ModR/M 16 = reg=DX rm=disp16.
            emit_group_sym_lea(&[0x0B, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::XorDxGroupSym { group, symbol, offset } => {
            // `xor dx,word ptr <group>:<symbol>` → 33 16 lo hi.
            // Opcode 33 (XOR r16,r/m16); ModR/M 16 = reg=DX rm=disp16.
            emit_group_sym_lea(&[0x33, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::XorAxGroupSym { group, symbol, offset } => {
            // `xor ax,word ptr <group>:<symbol>` → 33 06 lo hi.
            // Same opcode as the DX form; ModR/M reg field 000=AX.
            emit_group_sym_lea(&[0x33, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::CmpAxGroupSym { group, symbol, offset } => {
            // `cmp ax,word ptr <group>:<symbol>` → 3B 06 lo hi.
            // ModR/M 06 = mod=00 reg=AX(000) rm=110 (disp16-only).
            emit_group_sym_lea(&[0x3B, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::CmpDxGroupSym { group, symbol, offset } => {
            // `cmp dx,word ptr <group>:<symbol>` → 3B 16 lo hi.
            // Same opcode as the AX form; ModR/M reg field 010=DX.
            emit_group_sym_lea(&[0x3B, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::PushGroupSym { group, symbol, offset } => {
            // `push word ptr <group>:<sym>[+N]` → FF 36 lo hi.
            // FF /6 r/m16 with disp16-only addressing (ModR/M 36 =
            // mod=00 reg=110 rm=110).
            emit_group_sym_lea(&[0xFF, 0x36], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::PushBpRel { offset } => {
            // `push word ptr [bp+disp8]` → FF 76 dd.
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0xFF);
            out.push(0x76);
            out.push(disp as u8);
        }
        Instr::PushSiPtr => {
            // `push word ptr [si]` → FF 34.
            out.push(0xFF);
            out.push(0x34);
        }
        Instr::PushSiDisp { disp } => {
            // `push word ptr [si+disp8]` → FF 74 dd.
            out.push(0xFF);
            out.push(0x74);
            out.push(*disp as u8);
        }
        Instr::PushDs => {
            // `push ds` → 1E (single-byte segreg-push form).
            out.push(0x1E);
        }
        Instr::PushSs => {
            // `push ss` → 16 (single-byte segreg-push form).
            out.push(0x16);
        }
        Instr::MovReg16SegReg { dst, src } => {
            // `mov <reg16>, <segreg>` → 8C + ModR/M
            // (mod=11 reg=<sreg> r/m=<reg16>).
            out.push(0x8C);
            out.push(0b11_000_000 | (src.code() << 3) | dst.code());
        }
        Instr::CmpGroupSymImm8Sx { group, symbol, offset, imm } => {
            // `cmp word ptr <group>:<sym>[+N], imm8sx` → 83 3E lo hi ii.
            // Grp1 r/m16,imm8sx with /7=CMP and disp16-only addressing.
            // Long const-compare chained-cmp pattern (fixture 223).
            emit_group_sym_lea(&[0x83, 0x3E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm as u8);
        }
        Instr::CmpGroupSymImm16 { group, symbol, offset, imm } => {
            // `cmp word ptr <group>:<sym>[+N], imm16` → 81 3E lo hi imm_lo imm_hi.
            // Wider sibling for K outside i8sx range (fixture 282).
            emit_group_sym_lea(&[0x81, 0x3E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::CmpByteGroupSymImm8 { group, symbol, offset, imm } => {
            // `cmp byte ptr <group>:<sym>[+N], imm8` → 80 3E lo hi ii.
            // Grp1 r/m8,imm8 with /7=CMP for char-global compare-vs-const
            // (fixture 452).
            emit_group_sym_lea(&[0x80, 0x3E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm);
        }
        Instr::AddGroupSymImm8Sx { group, symbol, offset, imm } => {
            // `add word ptr <group>:<sym>[+N], imm8sx` → 83 06 lo hi ii.
            // Grp1 r/m16,imm8sx with /0=ADD (fixture 249's `g++` low half).
            emit_group_sym_lea(&[0x83, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm as u8);
        }
        Instr::AdcGroupSymImm8Sx { group, symbol, offset, imm } => {
            // `adc word ptr <group>:<sym>[+N], imm8sx` → 83 16 lo hi ii.
            // Grp1 r/m16,imm8sx with /2=ADC (fixture 249's `g++` high half).
            emit_group_sym_lea(&[0x83, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm as u8);
        }
        Instr::SubGroupSymImm8Sx { group, symbol, offset, imm } => {
            // `sub word ptr <group>:<sym>[+N], imm8sx` → 83 2E lo hi ii.
            // Grp1 r/m16,imm8sx with /5=SUB (fixture 250's `g--` low half).
            emit_group_sym_lea(&[0x83, 0x2E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm as u8);
        }
        Instr::SbbGroupSymImm8Sx { group, symbol, offset, imm } => {
            // `sbb word ptr <group>:<sym>[+N], imm8sx` → 83 1E lo hi ii.
            // Grp1 r/m16,imm8sx with /3=SBB (fixture 250's `g--` high half).
            emit_group_sym_lea(&[0x83, 0x1E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm as u8);
        }
        Instr::AndGroupSymImm16 { group, symbol, offset, imm } => {
            // `and word ptr <group>:<sym>[+N], imm16` → 81 26 lo hi imm_lo imm_hi.
            // Grp1 r/m16,imm16 with /4=AND (fixture 253's `g &= K`).
            emit_group_sym_lea(&[0x81, 0x26], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::OrGroupSymImm16 { group, symbol, offset, imm } => {
            // `or word ptr <group>:<sym>[+N], imm16` → 81 0E lo hi imm_lo imm_hi.
            // Grp1 r/m16,imm16 with /1=OR.
            emit_group_sym_lea(&[0x81, 0x0E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::XorGroupSymImm16 { group, symbol, offset, imm } => {
            // `xor word ptr <group>:<sym>[+N], imm16` → 81 36 lo hi imm_lo imm_hi.
            // Grp1 r/m16,imm16 with /6=XOR.
            emit_group_sym_lea(&[0x81, 0x36], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::Cbw => out.push(0x98),
        Instr::LeaReg16BpRel { dst, offset } => {
            // `lea r16,word ptr [bp+disp8]` → 8D xx dd. ModR/M xx =
            // mod=01 reg=<dst-code> r/m=110 ([bp+disp8]).
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x8D);
            out.push(0b01_000_110 | (dst.code() << 3));
            out.push(disp as u8);
        }
        Instr::MovSiPtrImm { imm } => {
            // `mov word ptr [si],imm16` → C7 04 lo hi. ModR/M 04 =
            // mod=00 /0 r/m=100 ([si]).
            out.push(0xC7);
            out.push(0x04);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovSiPtrImm8 { imm } => {
            // `mov byte ptr [si],imm8` → C6 04 ii. Same ModR/M as
            // the word-form, but the byte opcode (C6 vs C7) and a
            // single immediate byte.
            out.push(0xC6);
            out.push(0x04);
            out.push(*imm);
        }
        Instr::MovSiDispImm { disp, imm } => {
            // `mov word ptr [si+disp8],imm16` → C7 44 dd lo hi.
            // ModR/M 44 = mod=01 /0 r/m=100 ([si+disp8]).
            out.push(0xC7);
            out.push(0x44);
            out.push(*disp as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovAxSiDisp { disp } => {
            // `mov ax,word ptr [si+disp8]` → 8B 44 dd. ModR/M 44 =
            // mod=01 reg=AX r/m=100 ([si+disp8]).
            out.push(0x8B);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::MovDxFromSiPtr => {
            // `mov dx,word ptr [si]` → 8B 14. ModR/M 14 = mod=00
            // reg=DX r/m=100 ([si]).
            out.push(0x8B);
            out.push(0x14);
        }
        Instr::MovDxSiDisp { disp } => {
            // `mov dx,word ptr [si+disp8]` → 8B 54 dd.
            out.push(0x8B);
            out.push(0x54);
            out.push(*disp as u8);
        }
        Instr::AddSiPtrImm8 { imm } => {
            // `add word ptr [si],imm8 (sign-extended)` → 83 04 ii.
            // 83 is Grp1 r/m16,imm8-sx; /0 selects ADD; ModR/M 04 =
            // mod=00 /0 r/m=100 ([si]).
            out.push(0x83);
            out.push(0x04);
            out.push(*imm as u8);
        }
        Instr::AdcSiDispImm8 { disp, imm } => {
            // `adc word ptr [si+disp8],imm8sx` → 83 54 dd ii.
            // ModR/M 54 = mod=01 /2(ADC) r/m=100 ([si+disp8]).
            out.push(0x83);
            out.push(0x54);
            out.push(*disp as u8);
            out.push(*imm as u8);
        }
        Instr::AddSiPtrDx => {
            // `add word ptr [si],dx` → 01 14. ModR/M 14 = mod=00
            // reg=DX(010) r/m=100=SI.
            out.push(0x01);
            out.push(0x14);
        }
        Instr::AdcSiDispAx { disp } => {
            // `adc word ptr [si+disp8],ax` → 11 44 dd. ModR/M
            // 44 = mod=01 reg=AX(000) r/m=100=SI with disp8.
            out.push(0x11);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::SubSiPtrImm8 { imm } => {
            // `sub word ptr [si],imm8sx` → 83 2C ii.
            // ModR/M 2C = mod=00 /5(SUB) r/m=100.
            out.push(0x83);
            out.push(0x2C);
            out.push(*imm as u8);
        }
        Instr::SbbSiDispImm8 { disp, imm } => {
            // `sbb word ptr [si+disp8],imm8sx` → 83 5C dd ii.
            // ModR/M 5C = mod=01 /3(SBB) r/m=100.
            out.push(0x83);
            out.push(0x5C);
            out.push(*disp as u8);
            out.push(*imm as u8);
        }
        Instr::AddBxPtrImm8 { imm } => {
            // `add word ptr [bx],imm8 (sign-extended)` → 83 07 ii.
            // Same opcode + /0 as the SI sibling; only the rm field
            // changes (111 = [bx]).
            out.push(0x83);
            out.push(0x07);
            out.push(*imm as u8);
        }
        Instr::AddBpRelImm8 { offset, imm } => {
            // `add word ptr [bp+disp8],imm8 (sign-extended)` → 83 46 dd ii.
            // ModR/M 46 = mod=01 /0(ADD) r/m=110 ([bp+disp8]).
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x83);
            out.push(0x46);
            out.push(disp as u8);
            out.push(*imm as u8);
        }
        Instr::AdcBpRelImm8 { offset, imm } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x83);
            out.push(0x56);
            out.push(disp as u8);
            out.push(*imm as u8);
        }
        Instr::SubBpRelImm8 { offset, imm } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x83);
            out.push(0x6E);
            out.push(disp as u8);
            out.push(*imm as u8);
        }
        Instr::SbbBpRelImm8 { offset, imm } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x83);
            out.push(0x5E);
            out.push(disp as u8);
            out.push(*imm as u8);
        }
        Instr::AndBpRelImm16 { offset, imm } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x81);
            out.push(0x66);
            out.push(disp as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::OrBpRelImm16 { offset, imm } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x81);
            out.push(0x4E);
            out.push(disp as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::XorBpRelImm16 { offset, imm } => {
            let disp = i8::try_from(*offset).expect("bp-relative offset fits in i8");
            out.push(0x81);
            out.push(0x76);
            out.push(disp as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovAxFromSiPtr => {
            // `mov ax,word ptr [si]` → 8B 04. ModR/M 04 = mod=00
            // reg=AX r/m=100 ([si]).
            out.push(0x8B);
            out.push(0x04);
        }
        Instr::MovBxPtrImm { imm } => {
            // `mov word ptr [bx],imm16` → C7 07 lo hi. ModR/M 07 =
            // mod=00 /0 r/m=111 ([bx]).
            out.push(0xC7);
            out.push(0x07);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovAxFromBxPtr => {
            // `mov ax,word ptr [bx]` → 8B 07. ModR/M 07 = mod=00
            // reg=AX r/m=111 ([bx]).
            out.push(0x8B);
            out.push(0x07);
        }
        Instr::MovBxFromBxPtr => {
            // `mov bx,word ptr [bx]` → 8B 1F. ModR/M 1F = mod=00
            // reg=BX r/m=111 ([bx]). Same opcode as MovAxFromBxPtr;
            // only the reg field of ModR/M differs.
            out.push(0x8B);
            out.push(0x1F);
        }
        Instr::ShlReg16One { reg } => {
            // `shl r16,1` → D1 (mod=11 /4 r/m=<reg>). D1 is Grp2
            // r/m16,1; /4 selects SHL.
            out.push(0xD1);
            out.push(0b11_100_000 | reg.code());
        }
        Instr::RclReg16One { reg } => {
            // `rcl r16,1` → D1 (mod=11 /2 r/m=<reg>). Same Grp2 opcode
            // as SHL; /2 selects RCL.
            out.push(0xD1);
            out.push(0b11_010_000 | reg.code());
        }
        Instr::SarReg16One { reg } => {
            // `sar r16,1` → D1 (mod=11 /7 r/m=<reg>). Same Grp2 opcode
            // family; /7 selects SAR (signed shift right).
            out.push(0xD1);
            out.push(0b11_111_000 | reg.code());
        }
        Instr::ShrReg16One { reg } => {
            // `shr r16,1` → D1 (mod=11 /5 r/m=<reg>). Same Grp2 opcode
            // family; /5 selects SHR (logical shift right).
            out.push(0xD1);
            out.push(0b11_101_000 | reg.code());
        }
        Instr::RcrReg16One { reg } => {
            // `rcr r16,1` → D1 (mod=11 /3 r/m=<reg>). Same Grp2 opcode
            // family; /3 selects RCR.
            out.push(0xD1);
            out.push(0b11_011_000 | reg.code());
        }
        Instr::NegReg16 { reg } => {
            // `neg r16` → F7 (mod=11 /3 r/m=<reg>). F7 is Grp3 r/m16.
            out.push(0xF7);
            out.push(0b11_011_000 | reg.code());
        }
        Instr::NotReg16 { reg } => {
            // `not r16` → F7 (mod=11 /2 r/m=<reg>).
            out.push(0xF7);
            out.push(0b11_010_000 | reg.code());
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

/// Shared helper for `<op> {ax|al},<form>:<sym>` instructions where
/// the encoding is `<opcode-prefix> <16-bit-symbol-offset>` plus a
/// SegRelGroupTarget FIXUPP. Opcode-prefix length varies by op:
///   1 byte for `mov ax,moffs16` (A1), `mov al,moffs8` (A0), and
///   `mov ax,offset _sym` (B8). 2 bytes for `add ax,r/m16` with
///   disp16-only addressing (03 06).
fn emit_group_sym_lea(
    opcode_prefix: &[u8],
    group: &str,
    symbol: &str,
    extra_offset: i16,
    symbols: &Symbols,
    group_idx: &HashMap<String, u8>,
    extern_idx: &HashMap<String, u8>,
    out: &mut Vec<u8>,
    fixups: &mut Vec<FixupReq>,
) -> AsmResult<()> {
    let g_idx = *group_idx
        .get(group)
        .ok_or_else(|| AsmError::new(0, format!("group `{group}` not defined")))?;
    // The symbol may be either defined in a segment of this module
    // (BCC's own globals) or an extern (defined in another TU). The
    // FIXUPP target differs: SEGDEF vs EXTDEF. `extra_offset` is the
    // `+N` modifier on the operand (e.g. `_a+2` for `a[1]`); it's
    // added to the symbol's location before encoding.
    if let Some(sym_loc) = symbols.get(symbol) {
        let target_seg_idx = u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
        let value = sym_loc.offset.wrapping_add(extra_offset as u16);
        out.extend_from_slice(opcode_prefix);
        let imm_start = out.len();
        out.extend_from_slice(&value.to_le_bytes());
        fixups.push(FixupReq {
            data_offset: u16::try_from(imm_start).expect("offset fits"),
            kind: FixupKind::SegRelGroupTarget {
                group_idx: g_idx,
                segment_idx: target_seg_idx,
            },
        });
        return Ok(());
    }
    if let Some(&ext_idx) = extern_idx.get(symbol) {
        // Extern: offset bytes are zero (the linker patches them via
        // the EXTDEF). FIXUPP target method = 2 (EXTDEF no disp).
        // (Extern + `+N` offset isn't fixture-tested yet; would need
        // the linker to pre-compute the displacement.)
        if extra_offset != 0 {
            return Err(AsmError::new(
                0,
                format!("extern `{symbol}` with `+{extra_offset}` offset not supported"),
            ));
        }
        out.extend_from_slice(opcode_prefix);
        let imm_start = out.len();
        out.extend_from_slice(&0u16.to_le_bytes());
        fixups.push(FixupReq {
            data_offset: u16::try_from(imm_start).expect("offset fits"),
            kind: FixupKind::SegRelGroupExtern {
                group_idx: g_idx,
                extdef_idx: ext_idx,
            },
        });
        return Ok(());
    }
    Err(AsmError::new(
        0,
        format!("symbol `{symbol}` not defined in any segment"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Reg16, SegAlign, SegCombine};

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
            SegItem::Instr(Instr::PushReg16 { reg: Reg16::Bp }),
            SegItem::Instr(Instr::MovReg16Reg16 {
                dst: Reg16::Bp,
                src: Reg16::Sp,
            }),
            SegItem::Instr(Instr::XorReg16Reg16 {
                dst: Reg16::Ax,
                src: Reg16::Ax,
            }),
            SegItem::Instr(Instr::JmpShort("@1@50".into())),
            SegItem::Label("@1@50".into()),
            SegItem::Instr(Instr::PopReg16 { reg: Reg16::Bp }),
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
