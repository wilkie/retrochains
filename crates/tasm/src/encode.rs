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
        Instr::CmpReg16Imm8 { .. } | Instr::CmpAxImm { .. } | Instr::AddAxImm { .. } | Instr::SubAxImm { .. } => 3,
        Instr::CmpReg16Imm16 { .. } => 4,
        Instr::CmpBpRelImm8 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 1,
        Instr::CmpBpRelImm16 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 2,
        Instr::JmpShort(_) | Instr::ShlAxCl | Instr::SarAxCl | Instr::ShrAxCl => 2,
        Instr::ShlReg16Cl { .. } | Instr::SarReg16Cl { .. } | Instr::ShrReg16Cl { .. } => 2,
        Instr::ShlReg8Cl { .. } | Instr::SarReg8Cl { .. } | Instr::ShrReg8Cl { .. } => 2,
        Instr::Cwd => 1,
        Instr::JmpCondShort { .. } => 2,
        Instr::JmpIndirectCsTableBx { .. } => 5,
        Instr::JmpIndirectCsBxDisp { .. } => 4,
        Instr::LoopShort { .. } => 2,
        Instr::MovBpRelAx { offset } | Instr::MovBpRelReg16 { offset, .. } => {
            1 + bp_rel_modrm_size(*offset)
        }
        Instr::MovAxFromCsBx => 3,
        Instr::MovReg16OffsetSym { .. } => 3,
        Instr::MovReg16GroupSymBxDisp { .. } => 4,
        Instr::AddReg16GroupSymBxDisp { .. } => 4,
        Instr::CmpGroupSymBxDispImm8 { .. } => 5,
        Instr::IncGroupSymBxDisp { .. } | Instr::DecGroupSymBxDisp { .. }
        | Instr::IncGroupSymBxDispByte { .. } | Instr::DecGroupSymBxDispByte { .. } => 4,
        Instr::AddGroupSymBxDispImm8Sx { .. } | Instr::SubGroupSymBxDispImm8Sx { .. } => 5,
        Instr::AddGroupSymBxDispImm16 { .. } | Instr::SubGroupSymBxDispImm16 { .. } => 6,
        Instr::AddGroupSymBxDispReg16 { .. } | Instr::SubGroupSymBxDispReg16 { .. } => 4,
        Instr::CmpGroupSymBxDispImm16 { .. } => 6,
        Instr::CmpByteGroupSymBxDispImm8 { .. } => 5,
        Instr::MovReg8GroupSymBxDisp { .. } => 4,
        Instr::MovGroupSymBxDispReg8 { .. } => 4,
        Instr::MovGroupSymBxDispImm8 { .. } => 5,
        Instr::AddReg16GroupSym { .. } => 4,
        Instr::OrReg16GroupSym { .. } => 4,
        Instr::MovGroupSymBxDispImm { .. } => 6,
        Instr::MovGroupSymBxDispReg16 { .. } => 4,
        Instr::MovGroupSymSiDispByteImm8 { .. } => 5,
        Instr::MovGroupSymSiDispReg8 { .. } => 4,
        Instr::MovReg8GroupSymSiDisp { .. } => 4,
        Instr::MovReg16GroupSymSiDisp { .. } => 4,
        Instr::MovGroupSymSiDispReg16 { .. } => 4,
        Instr::MovGroupSymSiDispImm16 { .. } => 6,
        Instr::MovReg16Imm { .. } => 3,
        Instr::SubSpImm(imm) | Instr::AddSpImm(imm) => {
            if i8::try_from(*imm as i16).is_ok() { 3 } else { 4 }
        }
        Instr::MovReg16BpRel { offset, .. }
        | Instr::AddAxBpRel { offset }
        | Instr::AdcDxBpRel { offset }
        | Instr::SbbDxBpRel { offset }
        | Instr::AddDxBpRel { offset }
        | Instr::AdcAxBpRel { offset }
        | Instr::SubDxBpRel { offset }
        | Instr::SbbAxBpRel { offset }
        | Instr::AndDxBpRel { offset }
        | Instr::OrDxBpRel { offset }
        | Instr::XorDxBpRel { offset }
        | Instr::AddBpRelDx { offset }
        | Instr::AdcBpRelAx { offset }
        | Instr::SubBpRelDx { offset }
        | Instr::SbbBpRelAx { offset }
        | Instr::AndBpRelDx { offset }
        | Instr::AndBpRelAx { offset }
        | Instr::OrBpRelDx { offset }
        | Instr::OrBpRelAx { offset }
        | Instr::XorBpRelDx { offset }
        | Instr::XorBpRelAx { offset }
        | Instr::AddBpRelAx { offset }
        | Instr::AdcBpRelDx { offset }
        | Instr::SubBpRelAx { offset }
        | Instr::SbbBpRelDx { offset }
        | Instr::AddBpRelByteAl { offset }
        | Instr::SubBpRelByteAl { offset }
        | Instr::AndBpRelByteAl { offset }
        | Instr::OrBpRelByteAl { offset }
        | Instr::XorBpRelByteAl { offset }
        | Instr::SubAxBpRel { offset }
        | Instr::AndAxBpRel { offset }
        | Instr::AndReg16BpRel { offset, .. }
        | Instr::OrAxBpRel { offset }
        | Instr::OrReg16BpRel { offset, .. }
        | Instr::XorAxBpRel { offset }
        | Instr::XorReg16BpRel { offset, .. }
        | Instr::AddReg16BpRel { offset, .. }
        | Instr::SubReg16BpRel { offset, .. }
        | Instr::CmpAxBpRel { offset }
        | Instr::CmpDxBpRel { offset }
        | Instr::CmpReg16BpRel { offset, .. }
        | Instr::CmpBpRelReg16 { offset, .. }
        | Instr::ImulBpRel { offset }
        | Instr::IdivBpRel { offset }
        | Instr::DivBpRel { offset }
        | Instr::ImulByteBpRel { offset }
        | Instr::IdivByteBpRel { offset }
        | Instr::DivByteBpRel { offset }
        | Instr::MovReg8BpRel { offset, .. }
        | Instr::MovBpRelReg8 { offset, .. } => 1 + bp_rel_modrm_size(*offset),
        Instr::MovReg8Imm8 { .. } => 2,
        Instr::MovReg8Reg8 { .. } => 2,
        Instr::MovBpRelImm8 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 1,
        Instr::MovByteSiDispImm8 { disp, .. } => if *disp == 0 { 3 } else { 4 },
        Instr::MovReg8ByteSiDisp { disp, .. } => if *disp == 0 { 2 } else { 3 },
        Instr::IncReg8 { .. } | Instr::DecReg8 { .. } => 2,
        Instr::CmpReg8Imm8 { .. } => 3,
        Instr::CmpAlBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::AddAlImm8 { .. }
        | Instr::SubAlImm8 { .. }
        | Instr::AndAlImm8 { .. }
        | Instr::OrAlImm8 { .. }
        | Instr::XorAlImm8 { .. } => 2,
        Instr::AndReg8Imm8 { .. }
        | Instr::OrReg8Imm8 { .. }
        | Instr::XorReg8Imm8 { .. } => 3,
        Instr::AddReg8Reg8 { .. }
        | Instr::SubReg8Reg8 { .. }
        | Instr::AndReg8Reg8 { .. }
        | Instr::OrReg8Reg8 { .. }
        | Instr::XorReg8Reg8 { .. } => 2,
        Instr::CallNear(_) => 3,
        Instr::MovAxGroupSym { .. }
        | Instr::MovAlGroupSym { .. }
        | Instr::MovGroupSymAl { .. }
        | Instr::MovReg16OffsetGroupSym { .. } => 3,
        Instr::MovReg8GroupSym { .. } => 4,
        Instr::MovReg16WordGroupSym { .. } => 4,
        Instr::MovGroupSymImm16 { .. } => 6,
        Instr::MovGroupSymImm8 { .. } => 5,
        Instr::MovGroupSymOffsetGroupSym { .. } => 6,
        Instr::MovGroupSymAx { .. } => 3,
        Instr::MovGroupSymReg16 { .. } => 4,
        Instr::MovGroupSymReg8 { .. } => 4,
        Instr::AddReg16Imm8Sx { .. }
        | Instr::AdcReg16Imm8Sx { .. }
        | Instr::SbbReg16Imm8Sx { .. }
        | Instr::SubReg16Imm8Sx { .. } => 3,
        Instr::AddReg16Imm16 { .. } | Instr::SubReg16Imm16 { .. } => 4,
        Instr::OrReg16Imm16 { .. } | Instr::AndReg16Imm16 { .. } | Instr::XorReg16Imm16 { .. } => 4,
        Instr::AddGroupSymImm16 { .. } => 6,
        Instr::AdcAxImm16 { .. }
        | Instr::SbbAxImm16 { .. }
        | Instr::AndAxImm16 { .. }
        | Instr::OrAxImm16 { .. }
        | Instr::XorAxImm16 { .. } => 3,
        Instr::MovAlFromSiPtr | Instr::MovAlFromBxPtr | Instr::MovAlFromDiPtr => 2,
        Instr::MovAlFromBxSi | Instr::MovAlFromBxDi => 2,
        Instr::MovBxSiPtrImm8 { .. } | Instr::MovBxDiPtrImm8 { .. } => 3,
        Instr::ImulReg16 { .. } | Instr::IdivReg16 { .. } | Instr::DivReg16 { .. } => 2,
        Instr::AddAxOffsetGroupSym { .. } => 3,
        Instr::AddAxGroupSym { .. }
        | Instr::OrAxGroupSym { .. }
        | Instr::AddDxGroupSym { .. }
        | Instr::AdcAxGroupSym { .. }
        | Instr::AddGroupSymDx { .. }
        | Instr::AdcGroupSymAx { .. }
        | Instr::SbbGroupSymAx { .. }
        | Instr::AdcGroupSymDx { .. }
        | Instr::SbbGroupSymDx { .. }
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
        Instr::PushBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::PushSiDisp { .. } => 3,
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
        Instr::IncGroupSym { .. } | Instr::DecGroupSym { .. } => 4,
        Instr::TestGroupSymImm16 { .. } => 6,
        Instr::TestBpRelImm16 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 2,
        Instr::TestBpRelAx { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::TestReg16Imm16 { .. } => 4,
        Instr::TestReg16Reg16 { .. } => 2,
        Instr::AddGroupSymReg16 { .. } | Instr::SubGroupSymReg16 { .. } => 4,
        Instr::AndGroupSymReg16 { .. }
        | Instr::OrGroupSymReg16 { .. }
        | Instr::XorGroupSymReg16 { .. } => 4,
        Instr::AddGroupSymReg8 { .. }
        | Instr::SubGroupSymReg8 { .. }
        | Instr::AndGroupSymReg8 { .. }
        | Instr::OrGroupSymReg8 { .. }
        | Instr::XorGroupSymReg8 { .. } => 4,
        Instr::AndGroupSymImm8 { .. }
        | Instr::OrGroupSymImm8 { .. }
        | Instr::XorGroupSymImm8 { .. } => 5,
        Instr::IncBpRel { .. } | Instr::DecBpRel { .. } => 3,
        Instr::ShlGroupSymOne { .. }
        | Instr::SarGroupSymOne { .. }
        | Instr::ShrGroupSymOne { .. } => 4,
        Instr::ShlGroupSymByteOne { .. }
        | Instr::SarGroupSymByteOne { .. }
        | Instr::ShrGroupSymByteOne { .. } => 4,
        Instr::ShlGroupSymByteCl { .. }
        | Instr::SarGroupSymByteCl { .. }
        | Instr::ShrGroupSymByteCl { .. } => 4,
        Instr::ShlGroupSymCl { .. }
        | Instr::SarGroupSymCl { .. }
        | Instr::ShrGroupSymCl { .. } => 4,
        Instr::ImulGroupSym { .. } | Instr::IdivGroupSym { .. } => 4,
        Instr::ImulSiPtr | Instr::IdivSiPtr => 2,
        Instr::IncGroupSymByte { .. } | Instr::DecGroupSymByte { .. } => 4,
        Instr::IncBpRelByte { offset } | Instr::DecBpRelByte { offset } => {
            1 + bp_rel_modrm_size(*offset)
        }
        Instr::CmpByteBpRelImm8 { .. } => 4,
        Instr::CmpByteSiPtrImm8 { .. } | Instr::CmpByteBxPtrImm8 { .. } | Instr::CmpByteDiPtrImm8 { .. } => 3,
        Instr::CmpWordSiPtrImm8Sx { .. } | Instr::CmpWordDiPtrImm8Sx { .. } | Instr::CmpWordBxPtrImm8Sx { .. } => 3,
        Instr::CmpWordSiPtrImm16 { .. } | Instr::CmpWordDiPtrImm16 { .. } | Instr::CmpWordBxPtrImm16 { .. } => 4,
        Instr::CmpAxFromDiPtr
        | Instr::CmpAxFromSiPtr
        | Instr::CmpAxFromBxPtr
        | Instr::CmpAlFromSiPtr
        | Instr::CmpAlFromDiPtr
        | Instr::CmpAlFromBxPtr => 2,
        Instr::CmpWordSiDispImm8Sx { disp, .. } => if *disp == 0 { 3 } else { 4 },
        Instr::AndGroupSymImm16 { .. }
        | Instr::OrGroupSymImm16 { .. }
        | Instr::XorGroupSymImm16 { .. }
        | Instr::CmpGroupSymImm16 { .. } => 6,
        Instr::Cbw => 1,
        Instr::LeaReg16BpRel { offset, .. } => 1 + bp_rel_modrm_size(*offset),
        Instr::MovSiPtrImm { .. } | Instr::MovBxPtrImm { .. } | Instr::MovDiPtrImm { .. } => 4,
        Instr::MovBxPtrImm8 { .. } => 3,
        Instr::AddSiPtrImm16 { .. } => 4,
        Instr::XorDiPtrReg16 { .. } => 2,
        Instr::MovBxPtrAl | Instr::MovBxPtrAx => 2,
        Instr::MovBxPtrReg16 { .. } => 2,
        Instr::MovSiPtrImm8 { .. } => 3,
        Instr::MovSiPtrReg16 { .. } | Instr::MovDiPtrReg16 { .. } => 2,
        Instr::MovSiPtrReg8 { .. } | Instr::MovDiPtrReg8 { .. } => 2,
        Instr::MovSiDispImm { .. } => 5,
        Instr::MovSiDispReg16 { .. } => 3,
        Instr::MovAxSiDisp { .. } | Instr::MovDxSiDisp { .. } => 3,
        Instr::MovDxFromSiPtr => 2,
        Instr::MovReg16FromSiPtr { .. } => 2,
        Instr::MovReg16SiDisp { .. } => 3,
        Instr::MovReg16FromDiPtr { .. } => 2,
        Instr::MovReg16DiDisp { .. } => 3,
        Instr::AddSiPtrImm8 { .. } | Instr::AddBxPtrImm8 { .. } | Instr::SubSiPtrImm8 { .. } => 3,
        Instr::AndSiPtrByteImm8 { .. }
        | Instr::OrSiPtrByteImm8 { .. }
        | Instr::XorSiPtrByteImm8 { .. } => 3,
        Instr::AndBpRelByteImm8 { offset, .. }
        | Instr::OrBpRelByteImm8 { offset, .. }
        | Instr::XorBpRelByteImm8 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 1,
        Instr::AddSiPtrReg8 { .. } | Instr::SubSiPtrReg8 { .. } => 2,
        Instr::IncSiPtrByte | Instr::DecSiPtrByte => 2,
        Instr::IncSiPtrWord | Instr::DecSiPtrWord => 2,
        Instr::AdcSiDispImm8 { .. } | Instr::SbbSiDispImm8 { .. } => 4,
        Instr::AddSiPtrDx => 2,
        Instr::AddSiPtrAx
        | Instr::SubSiPtrAx
        | Instr::AndSiPtrAx
        | Instr::OrSiPtrAx
        | Instr::XorSiPtrAx
        | Instr::ShlSiPtrCl
        | Instr::SarSiPtrCl
        | Instr::ShrSiPtrCl => 2,
        Instr::AddBxDispAx { .. }
        | Instr::SubBxDispAx { .. }
        | Instr::AndBxDispAx { .. }
        | Instr::OrBxDispAx { .. }
        | Instr::XorBxDispAx { .. } => 3,
        Instr::AddSiDispAx { .. }
        | Instr::SubSiDispAx { .. }
        | Instr::AndSiDispAx { .. }
        | Instr::OrSiDispAx { .. }
        | Instr::XorSiDispAx { .. } => 3,
        Instr::AddBxDispImm8 { .. } | Instr::SubBxDispImm8 { .. } => 4,
        Instr::MovAlBxDisp { .. } | Instr::MovBxDispAl { .. } => 3,
        Instr::AndBxDispAl { .. }
        | Instr::OrBxDispAl { .. }
        | Instr::XorBxDispAl { .. } => 3,
        Instr::AndBxDispImm16 { .. }
        | Instr::OrBxDispImm16 { .. }
        | Instr::XorBxDispImm16 { .. } => 5,
        Instr::AddBxPtrAx
        | Instr::SubBxPtrAx
        | Instr::AndBxPtrAx
        | Instr::OrBxPtrAx
        | Instr::XorBxPtrAx => 2,
        Instr::IncBxDisp { .. } | Instr::DecBxDisp { .. } => 3,
        Instr::IncBxDispByte { .. } | Instr::DecBxDispByte { .. } => 3,
        Instr::CmpBxDispImm8 { .. } => 4,
        Instr::ShlBxDispImm1 { .. }
        | Instr::SarBxDispImm1 { .. }
        | Instr::ShrBxDispImm1 { .. } => 3,
        Instr::ShlBxDispCl { .. }
        | Instr::SarBxDispCl { .. }
        | Instr::ShrBxDispCl { .. } => 3,
        Instr::MovAxBxDisp { .. } | Instr::MovBxDispAx { .. } => 3,
        Instr::MovBxDispDx { .. } => 3,
        Instr::MovDxBxDisp { .. } => 3,
        Instr::MovBxDispImm { .. } => 5,
        Instr::AdcBxDispImm8 { .. } | Instr::SbbBxDispImm8 { .. } => 4,
        Instr::PushBxDisp { .. } => 3,
        Instr::AddAlBpRel { offset }
        | Instr::SubAlBpRel { offset }
        | Instr::AndAlBpRel { offset }
        | Instr::OrAlBpRel { offset }
        | Instr::XorAlBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::AdcSiDispAx { .. } => 3,
        Instr::AdcSiDispDx { .. } | Instr::SbbSiDispDx { .. } => 3,
        Instr::AddBpRelImm8 { offset, .. }
        | Instr::AdcBpRelImm8 { offset, .. }
        | Instr::SubBpRelImm8 { offset, .. }
        | Instr::SbbBpRelImm8 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 1,
        Instr::AndBpRelImm16 { offset, .. }
        | Instr::OrBpRelImm16 { offset, .. }
        | Instr::XorBpRelImm16 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 2,
        Instr::MovAxFromSiPtr
        | Instr::MovAxFromBxPtr
        | Instr::MovBxFromBxPtr
        | Instr::SubAxFromSiPtr
        | Instr::AddAxFromSiPtr
        | Instr::AddAxFromDiPtr
        | Instr::AddReg16FromBxPtr { .. }
        | Instr::AddReg16FromDiPtr { .. }
        | Instr::AddReg16FromSiPtr { .. } => 2,
        Instr::AddAxSiDisp { .. }
        | Instr::AddAxDiDisp { .. }
        | Instr::SubAxSiDisp { .. }
        | Instr::SubAxDiDisp { .. }
        | Instr::AddReg16SiDisp { .. }
        | Instr::AddReg16DiDisp { .. } => 3,
        Instr::ShlReg16One { .. }
        | Instr::RclReg16One { .. }
        | Instr::SarReg16One { .. }
        | Instr::ShrReg16One { .. }
        | Instr::RcrReg16One { .. }
        | Instr::NegReg16 { .. }
        | Instr::NotReg16 { .. }
        | Instr::ShlReg8One { .. }
        | Instr::SarReg8One { .. }
        | Instr::ShrReg8One { .. } => 2,
        Instr::MovBpRelImm { offset, .. }
        | Instr::MovBpRelOffsetSym { offset, .. }
        | Instr::MovBpRelOffsetGroupSym { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 2,
        Instr::CallIndirectBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        // 8087 FPU memory ops: TASM auto-prepends a `9B` (FWAIT)
        // prefix before each memory-form FPU instruction (matches
        // real TASM's 8087 compatibility behavior). The family
        // opcode (D9/DD) + ModR/M (+ disp) follow. Total = 1
        // (fwait) + 1 (family) + ModR/M + disp; group-symbol forms
        // are always disp16 (5 bytes total with the fwait).
        Instr::FldDwordBpRel { offset }
        | Instr::FstpDwordBpRel { offset }
        | Instr::FldQwordBpRel { offset }
        | Instr::FstpQwordBpRel { offset }
        | Instr::FpuArithBpRel { offset, .. }
        | Instr::FildWordBpRel { offset }
        | Instr::FcompBpRel { offset, .. }
        | Instr::FstswWordBpRel { offset } => 2 + bp_rel_modrm_size(*offset),
        Instr::FldDwordGroupSym { .. } | Instr::FldQwordGroupSym { .. }
        | Instr::FstpDwordGroupSym { .. } | Instr::FstpQwordGroupSym { .. }
        | Instr::FcompGroupSym { .. } | Instr::FpuArithGroupSym { .. } => 5,
        // Register-form FPU instructions: 9B (fwait) + family +
        // register-mode ModR/M = 3 bytes flat. No memory displacement.
        Instr::Fld1 | Instr::FsubpStack | Instr::Fchs => 3,
        // Standalone fwait emits `90 9B` (NOP + FWAIT) — TASM tags
        // the NOP byte with the FIWRQQ marker. 2 bytes total.
        Instr::Fwait => 2,
        Instr::Sahf => 1,
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
        Instr::CmpReg16Imm16 { reg, imm } => {
            // `cmp r16,imm16` → 81 (mod=11 /7 r/m=reg) lo hi.
            // 81 is Grp1 r/m16,imm16; /7 selects CMP.
            out.push(0x81);
            out.push(0b11_111_000 | reg.code());
            out.extend_from_slice(&imm.to_le_bytes());
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
        Instr::SubAxImm { imm } => {
            // `sub ax,imm16` → 2D lo hi.
            out.push(0x2D);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::CmpBpRelImm8 { offset, imm } => {
            // `cmp word ptr [bp+disp],imm8sx` → 83 /7 [bp+disp] ii.
            // ModR/M reg field 7 = CMP in Grp1.
            out.push(0x83);
            emit_bp_rel_modrm(7, *offset, out);
            out.push(*imm as u8);
        }
        Instr::CmpBpRelImm16 { offset, imm } => {
            // `cmp word ptr [bp+disp],imm16` → 81 /7 [bp+disp] lo hi.
            // Fixture 563.
            out.push(0x81);
            emit_bp_rel_modrm(7, *offset, out);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovReg16Imm { reg, imm } => {
            // `mov r16,imm16` → B8+rc lo hi.
            out.push(0xB8 | reg.code());
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::SubSpImm(imm) => {
            // imm8sx form when it fits (83 EC ii), imm16 otherwise
            // (81 EC lo hi). Fixture 1739 (200-byte frame uses
            // imm16).
            if let Ok(imm8) = i8::try_from(*imm as i16) {
                out.push(0x83);
                out.push(0xEC);
                out.push(imm8 as u8);
            } else {
                out.push(0x81);
                out.push(0xEC);
                out.extend_from_slice(&imm.to_le_bytes());
            }
        }
        Instr::AddSpImm(imm) => {
            // Sibling of SubSpImm. Used for cdecl stack cleanup
            // (fixture 138's `add sp,6`).
            if let Ok(imm8) = i8::try_from(*imm as i16) {
                out.push(0x83);
                out.push(0xC4);
                out.push(imm8 as u8);
            } else {
                out.push(0x81);
                out.push(0xC4);
                out.extend_from_slice(&imm.to_le_bytes());
            }
        }
        Instr::MovBpRelImm { offset, imm } => {
            // `mov word ptr [bp+disp], imm16` → C7 /0 [bp+disp] lo hi.
            out.push(0xC7);
            emit_bp_rel_modrm(0, *offset, out);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovBpRelOffsetGroupSym { offset, group, symbol, sym_offset } => {
            // `mov word ptr [bp+disp], offset <group>:<symbol>` →
            // C7 /0 [bp+disp] lo hi. The lo/hi imm is the symbol's
            // offset, FIXUPP-relocated like
            // `MovReg16OffsetGroupSym`.
            out.push(0xC7);
            emit_bp_rel_modrm(0, *offset, out);
            emit_group_sym_imm16(
                group, symbol, *sym_offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
        }
        Instr::MovReg16BpRel { reg, offset } => {
            // `mov r16, word ptr [bp+disp]` → 8B /<reg> [bp+disp].
            out.push(0x8B);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::AddAxBpRel { offset } => emit_alu_ax_bp_rel(0x03, *offset, out),
        Instr::SubAxBpRel { offset } => emit_alu_ax_bp_rel(0x2B, *offset, out),
        Instr::AdcDxBpRel { offset } => {
            // `adc dx, word ptr [bp+disp]` → 13 /DX [bp+disp].
            out.push(0x13);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::SbbDxBpRel { offset } => {
            out.push(0x1B);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::AddDxBpRel { offset } => {
            out.push(0x03);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::AdcAxBpRel { offset } => {
            out.push(0x13);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::SubDxBpRel { offset } => {
            out.push(0x2B);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::SbbAxBpRel { offset } => {
            out.push(0x1B);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::AndDxBpRel { offset } => {
            out.push(0x23);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::OrDxBpRel { offset } => {
            out.push(0x0B);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::XorDxBpRel { offset } => {
            out.push(0x33);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::AddBpRelDx { offset } => {
            // `add word ptr [bp+disp], dx` → 01 /DX [bp+disp]. ADD
            // r/m16, r16.
            out.push(0x01);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::AdcBpRelAx { offset } => {
            out.push(0x11);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::SubBpRelDx { offset } => {
            out.push(0x29);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::SbbBpRelAx { offset } => {
            out.push(0x19);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::AndBpRelDx { offset } => {
            out.push(0x21);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::AndBpRelAx { offset } => {
            out.push(0x21);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::OrBpRelDx { offset } => {
            out.push(0x09);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::OrBpRelAx { offset } => {
            out.push(0x09);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::XorBpRelDx { offset } => {
            out.push(0x31);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::XorBpRelAx { offset } => {
            out.push(0x31);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::AddBpRelAx { offset } => {
            out.push(0x01);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::AdcBpRelDx { offset } => {
            out.push(0x11);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::SubBpRelAx { offset } => {
            out.push(0x29);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::SbbBpRelDx { offset } => {
            out.push(0x19);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::AddBpRelByteAl { offset } => {
            out.push(0x00);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::SubBpRelByteAl { offset } => {
            out.push(0x28);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::AndBpRelByteAl { offset } => {
            out.push(0x20);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::OrBpRelByteAl { offset } => {
            out.push(0x08);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::XorBpRelByteAl { offset } => {
            out.push(0x30);
            emit_bp_rel_modrm(0, *offset, out);
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
        Instr::AddAxFromDiPtr => {
            // `add ax,word ptr [di]` → 03 05. ModR/M 05 = mod=00
            // reg=AX r/m=101 ([DI]).
            out.push(0x03);
            out.push(0x05);
        }
        Instr::AddReg16FromBxPtr { reg } => {
            // `add <reg16>, word ptr [bx]` → 03 (mod=00 reg=<r>
            // r/m=111). Memory-direct add through BX to any
            // (non-AX) register destination.
            out.push(0x03);
            out.push(0b00_000_111 | (reg.code() << 3));
        }
        Instr::AddReg16FromDiPtr { reg } => {
            // `add <reg16>, word ptr [di]` → 03 (mod=00 reg=<r>
            // r/m=101).
            out.push(0x03);
            out.push(0b00_000_101 | (reg.code() << 3));
        }
        Instr::AddReg16FromSiPtr { reg } => {
            // `add <reg16>, word ptr [si]` → 03 (mod=00 reg=<r>
            // r/m=100).
            out.push(0x03);
            out.push(0b00_000_100 | (reg.code() << 3));
        }
        Instr::AddAxSiDisp { disp } => {
            // `add ax,word ptr [si+disp8]` → 03 44 dd.
            out.push(0x03);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::AddAxDiDisp { disp } => {
            out.push(0x03);
            out.push(0x45);
            out.push(*disp as u8);
        }
        Instr::AddReg16SiDisp { reg, disp } => {
            // `add <reg16>, word ptr [si+disp8]` → 03 (mod=01
            // reg=<r> r/m=100) dd. Generic dst-reg sibling of
            // AddAxSiDisp.
            out.push(0x03);
            out.push(0b01_000_100 | (reg.code() << 3));
            out.push(*disp as u8);
        }
        Instr::AddReg16DiDisp { reg, disp } => {
            // `add <reg16>, word ptr [di+disp8]` → 03 (mod=01
            // reg=<r> r/m=101) dd.
            out.push(0x03);
            out.push(0b01_000_101 | (reg.code() << 3));
            out.push(*disp as u8);
        }
        Instr::SubAxSiDisp { disp } => {
            // `sub ax,word ptr [si+disp8]` → 2B 44 dd.
            out.push(0x2B);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::SubAxDiDisp { disp } => {
            out.push(0x2B);
            out.push(0x45);
            out.push(*disp as u8);
        }
        Instr::AndAxBpRel { offset } => emit_alu_ax_bp_rel(0x23, *offset, out),
        Instr::AndReg16BpRel { reg, offset } => {
            out.push(0x23);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::OrReg16BpRel { reg, offset } => {
            out.push(0x0B);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::XorReg16BpRel { reg, offset } => {
            out.push(0x33);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::AddReg16BpRel { reg, offset } => {
            out.push(0x03);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::SubReg16BpRel { reg, offset } => {
            out.push(0x2B);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::OrAxBpRel { offset } => emit_alu_ax_bp_rel(0x0B, *offset, out),
        Instr::XorAxBpRel { offset } => emit_alu_ax_bp_rel(0x33, *offset, out),
        Instr::CmpAxBpRel { offset } => emit_alu_ax_bp_rel(0x3B, *offset, out),
        Instr::CmpDxBpRel { offset } => {
            out.push(0x3B);
            emit_bp_rel_modrm(2, *offset, out);
        }
        Instr::CmpReg16BpRel { reg, offset } => {
            out.push(0x3B);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::CmpBpRelReg16 { reg, offset } => {
            // `cmp word ptr [bp+disp], <reg16>` → 39 (mod=xx
            // reg=<r> r/m=110) disp. Opcode 39 = CMP r/m16, r16
            // (memory-left direction).
            out.push(0x39);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::ImulBpRel { offset } => {
            // `imul word ptr [bp+disp]` → F7 /5 [bp+disp]. Grp3
            // /5=IMUL.
            out.push(0xF7);
            emit_bp_rel_modrm(5, *offset, out);
        }
        Instr::IdivBpRel { offset } => {
            // `idiv word ptr [bp+disp]` → F7 /7 [bp+disp].
            out.push(0xF7);
            emit_bp_rel_modrm(7, *offset, out);
        }
        Instr::DivBpRel { offset } => {
            // `div word ptr [bp+disp]` → F7 /6 [bp+disp].
            out.push(0xF7);
            emit_bp_rel_modrm(6, *offset, out);
        }
        Instr::ImulGroupSym { group, symbol, offset } => {
            // `imul word ptr <group>:<sym>[+N]` → F7 2E lo hi. Grp3
            // /5 (IMUL) with mod=00 r/m=110 → ModR/M = 0x2E. Fixture 809.
            emit_group_sym_lea(&[0xF7, 0x2E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::IdivGroupSym { group, symbol, offset } => {
            // `idiv word ptr <group>:<sym>[+N]` → F7 3E lo hi. Grp3
            // /7 (IDIV) with mod=00 r/m=110 → ModR/M = 0x3E. Fixture 810.
            emit_group_sym_lea(&[0xF7, 0x3E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::ImulSiPtr => {
            // `imul word ptr [si]` → F7 2C. Grp3 /5 (IMUL) with
            // mod=00 r/m=100 → ModR/M = 0x2C.
            out.push(0xF7);
            out.push(0x2C);
        }
        Instr::IdivSiPtr => {
            // `idiv word ptr [si]` → F7 3C. Grp3 /7 (IDIV) with
            // mod=00 r/m=100 → ModR/M = 0x3C. Fixture 825.
            out.push(0xF7);
            out.push(0x3C);
        }
        Instr::ImulByteBpRel { offset } => {
            // `imul byte ptr [bp+disp]` → F6 /5 [bp+disp]. Grp3 r/m8
            // /5=IMUL. Fixture 672.
            out.push(0xF6);
            emit_bp_rel_modrm(5, *offset, out);
        }
        Instr::IdivByteBpRel { offset } => {
            // `idiv byte ptr [bp+disp]` → F6 /7 [bp+disp]. Fixture 673.
            out.push(0xF6);
            emit_bp_rel_modrm(7, *offset, out);
        }
        Instr::DivByteBpRel { offset } => {
            // `div al, byte ptr [bp+disp]` → F6 /6 [bp+disp]. Fixture 677.
            out.push(0xF6);
            emit_bp_rel_modrm(6, *offset, out);
        }
        Instr::Cwd => out.push(0x99),
        Instr::MovReg8BpRel { reg, offset } => {
            // `mov <reg8>, byte ptr [bp+disp]` → 8A /<reg> [bp+disp].
            out.push(0x8A);
            emit_bp_rel_modrm(reg.code(), *offset, out);
        }
        Instr::MovBpRelReg8 { offset, reg } => {
            // `mov byte ptr [bp+disp], <reg8>` → 88 /<reg> [bp+disp].
            out.push(0x88);
            emit_bp_rel_modrm(reg.code(), *offset, out);
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
            // `mov byte ptr [bp+disp], imm8` → C6 /0 [bp+disp] ii.
            out.push(0xC6);
            emit_bp_rel_modrm(0, *offset, out);
            out.push(*imm);
        }
        Instr::MovReg8ByteSiDisp { reg, disp } => {
            // `mov reg8, byte ptr [si+disp]` — 8A (mod reg r/m=100).
            // disp=0 → mod=00 r/m=100 → 8A (00_reg_100); disp!=0 →
            // mod=01 r/m=100 → 8A (01_reg_100) dd.
            out.push(0x8A);
            if *disp == 0 {
                out.push(0b00_000_100 | (reg.code() << 3));
            } else {
                let d = i8::try_from(*disp).expect("si-rel disp fits in i8");
                out.push(0b01_000_100 | (reg.code() << 3));
                out.push(d as u8);
            }
        }
        Instr::MovByteSiDispImm8 { disp, imm } => {
            // `mov byte ptr [si+disp],imm8` — C6 (mod /0 r/m=100) ii.
            // disp=0 → mod=00 r/m=100 = 0x04 (3 bytes total);
            // disp!=0 → mod=01 r/m=100 = 0x44 with disp8 (4 bytes).
            out.push(0xC6);
            if *disp == 0 {
                out.push(0x04);
            } else {
                let d = i8::try_from(*disp).expect("si-rel disp fits in i8");
                out.push(0x44);
                out.push(d as u8);
            }
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
        Instr::CmpAlBpRel { offset } => {
            // `cmp al, byte ptr [bp+disp]` → 3A /AL [bp+disp]. CMP
            // r8, r/m8.
            out.push(0x3A);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::AddAlImm8 { imm } => {
            // `add al,imm8` → 04 ii. AL-specific accumulator form
            // (2 bytes). Fixture 529.
            out.push(0x04);
            out.push(*imm);
        }
        Instr::SubAlImm8 { imm } => {
            // `sub al,imm8` → 2C ii.
            out.push(0x2C);
            out.push(*imm);
        }
        Instr::AndAlImm8 { imm } => {
            // `and al,imm8` → 24 ii.
            out.push(0x24);
            out.push(*imm);
        }
        Instr::OrAlImm8 { imm } => {
            // `or al,imm8` → 0C ii.
            out.push(0x0C);
            out.push(*imm);
        }
        Instr::XorAlImm8 { imm } => {
            // `xor al,imm8` → 34 ii.
            out.push(0x34);
            out.push(*imm);
        }
        Instr::AndReg8Imm8 { reg, imm } => {
            // `and <reg8>,imm8` → 80 (mod=11 /4 r/m=<reg>) ii.
            // Grp1 r/m8,imm8 with /4=AND. Fixture 556.
            out.push(0x80);
            out.push(0b11_100_000 | reg.code());
            out.push(*imm);
        }
        Instr::OrReg8Imm8 { reg, imm } => {
            // `or <reg8>,imm8` → 80 (mod=11 /1 r/m=<reg>) ii.
            out.push(0x80);
            out.push(0b11_001_000 | reg.code());
            out.push(*imm);
        }
        Instr::XorReg8Imm8 { reg, imm } => {
            // `xor <reg8>,imm8` → 80 (mod=11 /6 r/m=<reg>) ii.
            out.push(0x80);
            out.push(0b11_110_000 | reg.code());
            out.push(*imm);
        }
        Instr::AddReg8Reg8 { dst, src } => {
            // `add <reg8>,<reg8>` → 02 (mod=11 reg=<dst> r/m=<src>).
            // Fixture 665 (`add dl, al` = 02 D0).
            out.push(0x02);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::SubReg8Reg8 { dst, src } => {
            // `sub <reg8>,<reg8>` → 2A (mod=11 reg=<dst> r/m=<src>).
            out.push(0x2A);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::AndReg8Reg8 { dst, src } => {
            // `and <reg8>,<reg8>` → 22 (mod=11 reg=<dst> r/m=<src>).
            out.push(0x22);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::OrReg8Reg8 { dst, src } => {
            // `or <reg8>,<reg8>` → 0A (mod=11 reg=<dst> r/m=<src>).
            out.push(0x0A);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::XorReg8Reg8 { dst, src } => {
            // `xor <reg8>,<reg8>` → 32 (mod=11 reg=<dst> r/m=<src>).
            out.push(0x32);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
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
        Instr::ShlReg16Cl { reg } => {
            // `shl <reg16>,cl` → D3 (mod=11 /4 r/m=<reg>). Fixture 537.
            out.push(0xD3);
            out.push(0b11_100_000 | reg.code());
        }
        Instr::SarReg16Cl { reg } => {
            // `sar <reg16>,cl` → D3 (mod=11 /7 r/m=<reg>).
            out.push(0xD3);
            out.push(0b11_111_000 | reg.code());
        }
        Instr::ShrReg16Cl { reg } => {
            // `shr <reg16>,cl` → D3 (mod=11 /5 r/m=<reg>).
            out.push(0xD3);
            out.push(0b11_101_000 | reg.code());
        }
        Instr::ShlReg8Cl { reg } => {
            // `shl <reg8>,cl` → D2 (mod=11 /4 r/m=<reg>).
            out.push(0xD2);
            out.push(0b11_100_000 | reg.code());
        }
        Instr::SarReg8Cl { reg } => {
            // `sar <reg8>,cl` → D2 (mod=11 /7 r/m=<reg>). Fixture 670
            // (`sar dl, cl` = D2 FA).
            out.push(0xD2);
            out.push(0b11_111_000 | reg.code());
        }
        Instr::ShrReg8Cl { reg } => {
            // `shr <reg8>,cl` → D2 (mod=11 /5 r/m=<reg>).
            out.push(0xD2);
            out.push(0b11_101_000 | reg.code());
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
            // `mov word ptr [bp+disp], ax` → 89 /AX [bp+disp]. MOV
            // r/m16, r16 (source/dest swap vs 8B).
            out.push(0x89);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::MovBpRelReg16 { offset, reg } => {
            // `mov word ptr [bp+disp], r16` → 89 /<reg> [bp+disp].
            out.push(0x89);
            emit_bp_rel_modrm(reg.code(), *offset, out);
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
        Instr::MovGroupSymOffsetGroupSym {
            dst_group, dst_symbol, dst_offset,
            src_group, src_symbol, src_offset,
        } => {
            // `mov word ptr <dg>:<dsym>[+N], offset <sg>:<ssym>` →
            // C7 06 <dst-disp> <src-imm>. Same shape as
            // MovGroupSymImm16 but the imm16 is *itself* a relocation
            // (offset of another global), so it carries its own
            // FIXUPP. Used by `p = &x;` between two globals
            // (fixture 480).
            emit_group_sym_lea(
                &[0xC7, 0x06], dst_group, dst_symbol, *dst_offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
            emit_group_sym_imm16(
                src_group, src_symbol, *src_offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
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
        Instr::SubReg16Imm8Sx { reg, imm } => {
            // `sub <reg16>, imm8sx` → 83 E(reg) ii. ModR/M E(reg) =
            // mod=11 /5(SUB) rm=<reg>.
            out.push(0x83);
            out.push(0b11_101_000 | reg.code());
            out.push(*imm as u8);
        }
        Instr::SubReg16Imm16 { reg, imm } => {
            // `sub <reg16>, imm16` → 81 E(reg) lo hi.
            out.push(0x81);
            out.push(0b11_101_000 | reg.code());
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::OrReg16Imm16 { reg, imm } => {
            // `or <reg16>, imm16` → 81 C(reg|8) lo hi. Grp1 /1=OR
            // with ModR/M mod=11 r/m=<reg>.
            out.push(0x81);
            out.push(0b11_001_000 | reg.code());
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::AndReg16Imm16 { reg, imm } => {
            // `and <reg16>, imm16` → 81 E(reg) lo hi. Grp1 /4=AND.
            out.push(0x81);
            out.push(0b11_100_000 | reg.code());
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::XorReg16Imm16 { reg, imm } => {
            // `xor <reg16>, imm16` → 81 F(reg) lo hi. Grp1 /6=XOR.
            out.push(0x81);
            out.push(0b11_110_000 | reg.code());
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
        Instr::AndAxImm16 { imm } => {
            // `and ax, imm16` → 25 lo hi. AX-specific accumulator
            // form (fixture 609's `c & 4` after cbw widening).
            out.push(0x25);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::OrAxImm16 { imm } => {
            // `or ax, imm16` → 0D lo hi. AX-specific accumulator
            // form (fixture 611's `x | 8`).
            out.push(0x0D);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::XorAxImm16 { imm } => {
            // `xor ax, imm16` → 35 lo hi. AX-specific accumulator
            // form (fixture 612's `x ^ 3`).
            out.push(0x35);
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
        Instr::AddReg16GroupSymBxDisp { reg, group, symbol, disp } => {
            // `add <reg16>,word ptr <group>:<sym>[bx+disp]` → 03
            // (mod=10 reg=<r> r/m=111) lo hi. Same FIXUPP shape as
            // MovReg16GroupSymBxDisp; opcode 03 (ADD r16,r/m16).
            let modrm = 0b10_000_111 | (reg.code() << 3);
            emit_group_sym_lea(&[0x03, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::IncGroupSymBxDisp { group, symbol, disp } => {
            emit_group_sym_lea(&[0xFF, 0x87], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::DecGroupSymBxDisp { group, symbol, disp } => {
            emit_group_sym_lea(&[0xFF, 0x8F], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::IncGroupSymBxDispByte { group, symbol, disp } => {
            emit_group_sym_lea(&[0xFE, 0x87], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::DecGroupSymBxDispByte { group, symbol, disp } => {
            emit_group_sym_lea(&[0xFE, 0x8F], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AddGroupSymBxDispImm8Sx { group, symbol, disp, imm } => {
            emit_group_sym_lea(&[0x83, 0x87], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm as u8);
        }
        Instr::AddGroupSymBxDispImm16 { group, symbol, disp, imm } => {
            emit_group_sym_lea(&[0x81, 0x87], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push((*imm & 0xFF) as u8); out.push((*imm >> 8) as u8);
        }
        Instr::SubGroupSymBxDispImm8Sx { group, symbol, disp, imm } => {
            emit_group_sym_lea(&[0x83, 0xAF], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm as u8);
        }
        Instr::SubGroupSymBxDispImm16 { group, symbol, disp, imm } => {
            emit_group_sym_lea(&[0x81, 0xAF], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push((*imm & 0xFF) as u8); out.push((*imm >> 8) as u8);
        }
        Instr::AddGroupSymBxDispReg16 { reg, group, symbol, disp } => {
            let modrm = 0b10_000_111 | (reg.code() << 3);
            emit_group_sym_lea(&[0x01, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SubGroupSymBxDispReg16 { reg, group, symbol, disp } => {
            let modrm = 0b10_000_111 | (reg.code() << 3);
            emit_group_sym_lea(&[0x29, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::CmpGroupSymBxDispImm8 { group, symbol, disp, imm } => {
            // `cmp word ptr <group>:<sym>[bx], imm8sx` → 83 BF lo
            // hi ii. ModR/M 0xBF = mod=10 reg=/7(cmp) r/m=111(bx+
            // disp16). disp16 is FIXUPP-patched.
            emit_group_sym_lea(&[0x83, 0xBF], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm as u8);
        }
        Instr::CmpGroupSymBxDispImm16 { group, symbol, disp, imm } => {
            // `cmp word ptr <group>:<sym>[bx], imm16` → 81 BF lo
            // hi LL HH.
            emit_group_sym_lea(&[0x81, 0xBF], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push((*imm & 0xFF) as u8);
            out.push((*imm >> 8) as u8);
        }
        Instr::CmpByteGroupSymBxDispImm8 { group, symbol, disp, imm } => {
            // `cmp byte ptr <group>:<sym>[bx], imm8` → 80 BF lo hi ii.
            emit_group_sym_lea(&[0x80, 0xBF], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm);
        }
        Instr::MovReg8GroupSymBxDisp { reg, group, symbol, disp } => {
            // `mov <reg8>, byte ptr <group>:<sym>[bx+disp]` → 8A
            // (mod=10 reg=<r> r/m=111) lo hi.
            let modrm = 0b10_000_111 | (reg.code() << 3);
            emit_group_sym_lea(&[0x8A, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymBxDispReg8 { reg, group, symbol, disp } => {
            // `mov byte ptr <group>:<sym>[bx+disp], <reg8>` → 88
            // (mod=10 reg=<r> r/m=111) lo hi.
            let modrm = 0b10_000_111 | (reg.code() << 3);
            emit_group_sym_lea(&[0x88, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymBxDispImm8 { group, symbol, disp, imm } => {
            // `mov byte ptr <group>:<sym>[bx+disp], imm8` → C6 87
            // lo hi ii.
            emit_group_sym_lea(&[0xC6, 0x87], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm);
        }
        Instr::AddReg16GroupSym { reg, group, symbol, offset } => {
            // `add <reg16>,word ptr <group>:<sym>[+offset]` → 03
            // (mod=00 reg=<r> r/m=110) lo hi.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x03, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::OrReg16GroupSym { reg, group, symbol, offset } => {
            // `or <reg16>,word ptr <group>:<sym>[+offset]` → 0B
            // (mod=00 reg=<r> r/m=110) lo hi.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x0B, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymBxDispImm { group, symbol, disp, imm } => {
            // `mov word ptr <group>:<sym>[bx+disp],imm16` → C7 87
            // lo hi imm_lo imm_hi.
            emit_group_sym_lea(&[0xC7, 0x87], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovGroupSymBxDispReg16 { group, symbol, disp, reg } => {
            // `mov word ptr <group>:<sym>[bx+disp],reg16` →
            // 89 (mod=10 reg=rrr r/m=111) lo hi. Fixture 510.
            let modrm = 0b10_000_111 | (reg.code() << 3);
            emit_group_sym_lea(&[0x89, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymSiDispByteImm8 { group, symbol, disp, imm } => {
            // `mov byte ptr <group>:<sym>[si+disp], imm8` → C6 84
            // lo hi ii. ModR/M 84 = mod=10 /0 r/m=100 ([SI]+disp16).
            emit_group_sym_lea(&[0xC6, 0x84], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm);
        }
        Instr::MovGroupSymSiDispReg8 { group, symbol, disp, reg } => {
            // `mov byte ptr <group>:<sym>[si+disp], <reg8>` → 88
            // (mod=10 reg=<r> r/m=100) lo hi.
            let modrm = 0b10_000_100 | (reg.code() << 3);
            emit_group_sym_lea(&[0x88, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovReg8GroupSymSiDisp { reg, group, symbol, disp } => {
            // `mov <reg8>, byte ptr <group>:<sym>[si+disp]` → 8A
            // (mod=10 reg=<r> r/m=100) lo hi.
            let modrm = 0b10_000_100 | (reg.code() << 3);
            emit_group_sym_lea(&[0x8A, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovReg16GroupSymSiDisp { reg, group, symbol, disp } => {
            // `mov <reg16>, word ptr <group>:<sym>[si+disp]` → 8B
            // (mod=10 reg=<r> r/m=100) lo hi.
            let modrm = 0b10_000_100 | (reg.code() << 3);
            emit_group_sym_lea(&[0x8B, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymSiDispReg16 { group, symbol, disp, reg } => {
            // `mov word ptr <group>:<sym>[si+disp], <reg16>` → 89
            // (mod=10 reg=<r> r/m=100) lo hi.
            let modrm = 0b10_000_100 | (reg.code() << 3);
            emit_group_sym_lea(&[0x89, modrm], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymSiDispImm16 { group, symbol, disp, imm } => {
            // `mov word ptr <group>:<sym>[si+disp], imm16` → C7 84
            // lo hi imm_lo imm_hi.
            emit_group_sym_lea(&[0xC7, 0x84], group, symbol, *disp as i16, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovAlGroupSym { group, symbol, offset } => {
            // `mov al,byte ptr <group>:<symbol>` → A0 lo hi.
            // A0 is the 8-bit moffs8 sibling of A1.
            emit_group_sym_lea(&[0xA0], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovReg8GroupSym { reg, group, symbol, offset } => {
            // `mov <reg8>,byte ptr <group>:<symbol>` → 8A (mod=00
            // reg=<r> r/m=110) lo hi. Generic byte load for non-AL
            // destinations. Fixture 739.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x8A, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymAl { group, symbol, offset } => {
            // `mov byte ptr <group>:<symbol>, al` → A2 lo hi.
            // A2 is the moffs8 store sibling of A0/A3. Fixture 683.
            emit_group_sym_lea(&[0xA2], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::MovGroupSymReg8 { group, symbol, offset, reg } => {
            // `mov byte ptr <group>:<symbol>, <reg8>` (non-AL) →
            // 88 (mod=00 reg=<r> r/m=110) lo hi. Fixture 692 stores
            // DL (idiv remainder low byte) → `88 16 lo hi`.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x88, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
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
        Instr::MovAlFromBxSi => {
            // `mov al,byte ptr [bx+si]` → 8A 00.
            out.push(0x8A); out.push(0x00);
        }
        Instr::MovAlFromBxDi => {
            // `mov al,byte ptr [bx+di]` → 8A 01.
            out.push(0x8A); out.push(0x01);
        }
        Instr::MovBxSiPtrImm8 { imm } => {
            // `mov byte ptr [bx+si], imm8` → C6 00 ii.
            out.push(0xC6); out.push(0x00); out.push(*imm);
        }
        Instr::MovBxDiPtrImm8 { imm } => {
            // `mov byte ptr [bx+di], imm8` → C6 01 ii.
            out.push(0xC6); out.push(0x01); out.push(*imm);
        }
        Instr::MovAlFromBxPtr => {
            // `mov al,byte ptr [bx]` → 8A 07. Same opcode as the
            // SI form; ModR/M 07 = mod=00 reg=AL r/m=111([bx]).
            out.push(0x8A);
            out.push(0x07);
        }
        Instr::MovAlFromDiPtr => {
            // `mov al,byte ptr [di]` → 8A 05. ModR/M 05 = mod=00
            // reg=AL r/m=101([di]).
            out.push(0x8A);
            out.push(0x05);
        }
        Instr::ImulReg16 { reg } => {
            // `imul r16` → F7 (mod=11 /5 r/m=<reg>).
            out.push(0xF7);
            out.push(0b11_101_000 | reg.code());
        }
        Instr::IdivReg16 { reg } => {
            // `idiv r16` → F7 (mod=11 /7 r/m=<reg>).
            out.push(0xF7);
            out.push(0b11_111_000 | reg.code());
        }
        Instr::DivReg16 { reg } => {
            // `div r16` → F7 (mod=11 /6 r/m=<reg>). Fixture 948.
            out.push(0xF7);
            out.push(0b11_110_000 | reg.code());
        }
        Instr::AddAxGroupSym { group, symbol, offset } => {
            // `add ax,word ptr <group>:<symbol>` → 03 06 lo hi.
            // ModR/M 06 = mod=00 reg=AX r/m=110 (disp16-only addressing).
            emit_group_sym_lea(&[0x03, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AddAxOffsetGroupSym { group, symbol, offset } => {
            // `add ax,offset <group>:<symbol>` → 05 lo hi (AX-accumulator
            // ADD-imm16). Same FIXUPP shape as `MovReg16OffsetGroupSym`:
            // the imm16 word is a link-time-resolved symbol offset.
            emit_group_sym_lea(&[0x05], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
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
        Instr::SbbGroupSymAx { group, symbol, offset } => {
            // `sbb word ptr <group>:<symbol>,ax` → 19 06 lo hi.
            // Opcode 19 (SBB r/m16,r16); ModR/M 06 = mod=00 reg=AX
            // rm=110. High-half borrow partner for `g -= h` long
            // global compound (fixture 735).
            emit_group_sym_lea(&[0x19, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AdcGroupSymDx { group, symbol, offset } => {
            // `adc word ptr <group>:<symbol>,dx` → 11 16 lo hi.
            // ModR/M 16 = mod=00 reg=DX rm=110. Fixture 755.
            emit_group_sym_lea(&[0x11, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SbbGroupSymDx { group, symbol, offset } => {
            // `sbb word ptr <group>:<symbol>,dx` → 19 16 lo hi.
            emit_group_sym_lea(&[0x19, 0x16], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
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
            // `push word ptr [bp+disp]` → FF /6 [bp+disp].
            out.push(0xFF);
            emit_bp_rel_modrm(6, *offset, out);
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
        Instr::CmpByteBpRelImm8 { offset, imm } => {
            // `cmp byte ptr [bp+disp8], imm8` → 80 7E dd ii.
            // ModR/M 7E = mod=01 reg=111(/7=CMP) r/m=110([bp]+disp8).
            // Fixture 524.
            out.push(0x80);
            out.push(0x7E);
            out.push(*offset as u8);
            out.push(*imm);
        }
        Instr::CmpByteSiPtrImm8 { imm } => {
            // `cmp byte ptr [si], imm8` → 80 3C ii.
            // ModR/M 3C = mod=00 reg=111(/7=CMP) r/m=100 ([si]).
            // Fixture 636.
            out.push(0x80);
            out.push(0x3C);
            out.push(*imm);
        }
        Instr::CmpWordSiPtrImm8Sx { imm } => {
            // `cmp word ptr [si], imm8sx` → 83 3C ii.
            out.push(0x83); out.push(0x3C); out.push(*imm as u8);
        }
        Instr::CmpWordDiPtrImm8Sx { imm } => {
            out.push(0x83); out.push(0x3D); out.push(*imm as u8);
        }
        Instr::CmpWordBxPtrImm8Sx { imm } => {
            out.push(0x83); out.push(0x3F); out.push(*imm as u8);
        }
        Instr::CmpWordSiPtrImm16 { imm } => {
            out.push(0x81); out.push(0x3C);
            out.push((*imm & 0xFF) as u8); out.push((*imm >> 8) as u8);
        }
        Instr::CmpWordDiPtrImm16 { imm } => {
            out.push(0x81); out.push(0x3D);
            out.push((*imm & 0xFF) as u8); out.push((*imm >> 8) as u8);
        }
        Instr::CmpWordBxPtrImm16 { imm } => {
            out.push(0x81); out.push(0x3F);
            out.push((*imm & 0xFF) as u8); out.push((*imm >> 8) as u8);
        }
        Instr::CmpByteBxPtrImm8 { imm } => {
            // `cmp byte ptr [bx], imm8` → 80 3F ii.
            // ModR/M 3F = mod=00 reg=111(/7=CMP) r/m=111 ([bx]).
            out.push(0x80);
            out.push(0x3F);
            out.push(*imm);
        }
        Instr::CmpByteDiPtrImm8 { imm } => {
            // `cmp byte ptr [di], imm8` → 80 3D ii.
            // ModR/M 3D = mod=00 reg=111(/7=CMP) r/m=101 ([di]).
            out.push(0x80);
            out.push(0x3D);
            out.push(*imm);
        }
        Instr::CmpAxFromDiPtr => {
            // `cmp ax, word ptr [di]` → 3B 05. ModR/M 05 = mod=00
            // reg=AX r/m=101 ([di]).
            out.push(0x3B);
            out.push(0x05);
        }
        Instr::CmpAxFromSiPtr => {
            // `cmp ax, word ptr [si]` → 3B 04. ModR/M 04 = mod=00
            // reg=AX r/m=100 ([si]).
            out.push(0x3B);
            out.push(0x04);
        }
        Instr::CmpAxFromBxPtr => {
            // `cmp ax, word ptr [bx]` → 3B 07. ModR/M 07 = mod=00
            // reg=AX r/m=111 ([bx]).
            out.push(0x3B);
            out.push(0x07);
        }
        Instr::CmpAlFromSiPtr => {
            // `cmp al, byte ptr [si]` → 3A 04.
            out.push(0x3A);
            out.push(0x04);
        }
        Instr::CmpAlFromDiPtr => {
            // `cmp al, byte ptr [di]` → 3A 05.
            out.push(0x3A);
            out.push(0x05);
        }
        Instr::CmpAlFromBxPtr => {
            // `cmp al, byte ptr [bx]` → 3A 07.
            out.push(0x3A);
            out.push(0x07);
        }
        Instr::CmpWordSiDispImm8Sx { disp, imm } => {
            // `cmp word ptr [si+disp], imm8sx` → Grp1 /7 r/m16,imm8sx.
            // Opcode 83. ModR/M: disp=0 → mod=00 r/m=100 = 0x3C;
            // disp!=0 → mod=01 r/m=100 = 0x7C with disp8.
            out.push(0x83);
            if *disp == 0 {
                out.push(0x3C);
            } else {
                let d = i8::try_from(*disp).expect("si-rel disp fits in i8");
                out.push(0x7C);
                out.push(d as u8);
            }
            out.push(*imm as u8);
        }
        Instr::AddGroupSymReg16 { group, symbol, offset, reg } => {
            // `add word ptr <group>:<sym>[+N], reg16` → 01 (mod=00
            // reg=<r> r/m=110) lo hi. Fixture 571.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x01, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SubGroupSymReg16 { group, symbol, offset, reg } => {
            // `sub word ptr <group>:<sym>[+N], reg16` → 29 (mod=00
            // reg=<r> r/m=110) lo hi.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x29, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AndGroupSymReg16 { group, symbol, offset, reg } => {
            // `and word ptr <group>:<sym>[+N], reg16` → 21 (mod=00
            // reg=<r> r/m=110) lo hi. Fixture 736.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x21, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::OrGroupSymReg16 { group, symbol, offset, reg } => {
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x09, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::XorGroupSymReg16 { group, symbol, offset, reg } => {
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x31, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AddGroupSymReg8 { group, symbol, offset, reg } => {
            // `add byte ptr <group>:<sym>[+N], reg8` → 00 (mod=00
            // reg=<r> r/m=110) lo hi. Fixture 680.
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x00, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SubGroupSymReg8 { group, symbol, offset, reg } => {
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x28, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AndGroupSymReg8 { group, symbol, offset, reg } => {
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x20, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::OrGroupSymReg8 { group, symbol, offset, reg } => {
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x08, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::XorGroupSymReg8 { group, symbol, offset, reg } => {
            let modrm = 0b00_000_110 | (reg.code() << 3);
            emit_group_sym_lea(&[0x30, modrm], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::AndGroupSymImm8 { group, symbol, offset, imm } => {
            // `and byte ptr <group>:<sym>[+N], imm8` → 80 26 lo hi ii.
            // Grp1 r/m8 imm8 with /4=AND, mod=00 r/m=110. Fixture 685.
            emit_group_sym_lea(&[0x80, 0x26], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm);
        }
        Instr::OrGroupSymImm8 { group, symbol, offset, imm } => {
            emit_group_sym_lea(&[0x80, 0x0E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm);
        }
        Instr::XorGroupSymImm8 { group, symbol, offset, imm } => {
            emit_group_sym_lea(&[0x80, 0x36], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.push(*imm);
        }
        Instr::TestGroupSymImm16 { group, symbol, offset, imm } => {
            // `test word ptr <group>:<sym>[+N], imm16` →
            // F7 06 lo hi imm_lo imm_hi. Grp3 /0=TEST r/m16, imm16.
            // Fixture 569.
            emit_group_sym_lea(&[0xF7, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::TestReg16Imm16 { reg, imm } => {
            // `test <reg16>, imm16` → F7 (C0+reg) imm_lo imm_hi.
            // Grp3 /0=TEST with mod=11 r/m=reg. Fixture 1415.
            out.push(0xF7);
            out.push(0xC0 | reg.code());
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::TestReg16Reg16 { dst, src } => {
            // `test <dst>, <src>` → 85 (mod=11 reg=dst r/m=src).
            // TASM puts the FIRST operand in the reg field and the
            // SECOND in the r/m field (TEST is symmetric so either
            // encoding sets identical flags, but the byte form
            // matters for byte-exact match). Fixture 3452 expects
            // `test ax, dx` → `85 C2` (reg=AX=000, r/m=DX=010).
            out.push(0x85);
            out.push(0b11_000_000 | (dst.code() << 3) | src.code());
        }
        Instr::TestBpRelImm16 { offset, imm } => {
            // `test word ptr [bp+disp8], imm16` → F7 46 dd lo hi.
            // ModR/M 46 = mod=01 /0(TEST) r/m=110([BP]+disp8).
            // Fixture 1853.
            out.push(0xF7);
            out.push(0x46);
            out.push(*offset as i8 as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::TestBpRelAx { offset } => {
            // `test word ptr [bp+disp8], ax` → 85 46 dd.
            out.push(0x85);
            out.push(0x46);
            out.push(*offset as i8 as u8);
        }
        Instr::IncGroupSym { group, symbol, offset } => {
            // `inc word ptr <group>:<sym>[+N]` → FF 06 lo hi.
            // Grp5 /0=INC r/m16 with mod=00 r/m=110 → `[disp16]`.
            // Fixture 512.
            emit_group_sym_lea(&[0xFF, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::IncBpRel { offset } => {
            // `inc word ptr [bp+disp8]` → FF 46 dd.
            // ModR/M 46 = mod=01 reg=000(/0=INC) r/m=110([bp]+disp8).
            // Fixture 547.
            out.push(0xFF);
            out.push(0x46);
            out.push(*offset as u8);
        }
        Instr::DecBpRel { offset } => {
            // `dec word ptr [bp+disp8]` → FF 4E dd. /1=DEC.
            out.push(0xFF);
            out.push(0x4E);
            out.push(*offset as u8);
        }
        Instr::ShlGroupSymOne { group, symbol, offset } => {
            // `shl word ptr <group>:<sym>[+N],1` → D1 26 lo hi.
            emit_group_sym_lea(&[0xD1, 0x26], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SarGroupSymOne { group, symbol, offset } => {
            // `sar word ptr <group>:<sym>[+N],1` → D1 3E lo hi.
            emit_group_sym_lea(&[0xD1, 0x3E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::ShrGroupSymOne { group, symbol, offset } => {
            // `shr word ptr <group>:<sym>[+N],1` → D1 2E lo hi.
            emit_group_sym_lea(&[0xD1, 0x2E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::ShlGroupSymByteOne { group, symbol, offset } => {
            // `shl byte ptr <group>:<sym>[+N],1` → D0 26 lo hi.
            // Fixture 688.
            emit_group_sym_lea(&[0xD0, 0x26], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SarGroupSymByteOne { group, symbol, offset } => {
            // `sar byte ptr <group>:<sym>[+N],1` → D0 3E lo hi.
            emit_group_sym_lea(&[0xD0, 0x3E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::ShrGroupSymByteOne { group, symbol, offset } => {
            // `shr byte ptr <group>:<sym>[+N],1` → D0 2E lo hi.
            emit_group_sym_lea(&[0xD0, 0x2E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::ShlGroupSymCl { group, symbol, offset } => {
            // `shl word ptr <group>:<sym>[+N],cl` → D3 26 lo hi.
            // Fixture 805.
            emit_group_sym_lea(&[0xD3, 0x26], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SarGroupSymCl { group, symbol, offset } => {
            // `sar word ptr <group>:<sym>[+N],cl` → D3 3E lo hi.
            emit_group_sym_lea(&[0xD3, 0x3E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::ShrGroupSymCl { group, symbol, offset } => {
            // `shr word ptr <group>:<sym>[+N],cl` → D3 2E lo hi.
            emit_group_sym_lea(&[0xD3, 0x2E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::ShlGroupSymByteCl { group, symbol, offset } => {
            // `shl byte ptr <group>:<sym>[+N],cl` → D2 26 lo hi.
            // Fixture 697.
            emit_group_sym_lea(&[0xD2, 0x26], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::SarGroupSymByteCl { group, symbol, offset } => {
            // `sar byte ptr <group>:<sym>[+N],cl` → D2 3E lo hi.
            emit_group_sym_lea(&[0xD2, 0x3E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::ShrGroupSymByteCl { group, symbol, offset } => {
            // `shr byte ptr <group>:<sym>[+N],cl` → D2 2E lo hi.
            emit_group_sym_lea(&[0xD2, 0x2E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::IncGroupSymByte { group, symbol, offset } => {
            // `inc byte ptr <group>:<sym>[+N]` → FE 06 lo hi.
            // Grp4 /0 r/m8 with mod=00 r/m=110. Fixture 702.
            emit_group_sym_lea(&[0xFE, 0x06], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::DecGroupSymByte { group, symbol, offset } => {
            // `dec byte ptr <group>:<sym>[+N]` → FE 0E lo hi.
            emit_group_sym_lea(&[0xFE, 0x0E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
        }
        Instr::IncBpRelByte { offset } => {
            // `inc byte ptr [bp+disp]` → FE /0 [bp+disp]. Grp4 /0
            // r/m8. Fixture 721.
            out.push(0xFE);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::DecBpRelByte { offset } => {
            // `dec byte ptr [bp+disp]` → FE /1 [bp+disp].
            out.push(0xFE);
            emit_bp_rel_modrm(1, *offset, out);
        }
        Instr::DecGroupSym { group, symbol, offset } => {
            // `dec word ptr <group>:<sym>[+N]` → FF 0E lo hi.
            emit_group_sym_lea(&[0xFF, 0x0E], group, symbol, *offset, symbols, group_idx, extern_idx, out, fixups)?;
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
            // `lea r16, word ptr [bp+disp]` → 8D /<dst> [bp+disp].
            out.push(0x8D);
            emit_bp_rel_modrm(dst.code(), *offset, out);
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
        Instr::MovSiPtrReg16 { src } => {
            // `mov word ptr [si],<reg16>` → 89 (mod=00 reg=<src>
            // r/m=100). ModR/M low 3 bits encode r/m=100 ([si]).
            out.push(0x89);
            out.push(0b00_000_100 | (src.code() << 3));
        }
        Instr::MovSiPtrReg8 { src } => {
            // `mov byte ptr [si],<reg8>` → 88 (mod=00 reg=<src>
            // r/m=100). Byte sibling of MovSiPtrReg16. Fixture 710.
            out.push(0x88);
            out.push(0b00_000_100 | (src.code() << 3));
        }
        Instr::MovDiPtrReg8 { src } => {
            // `mov byte ptr [di],<reg8>` → 88 (mod=00 reg=<src>
            // r/m=101). Byte sibling of MovDiPtrReg16. Fixture 3529.
            out.push(0x88);
            out.push(0b00_000_101 | (src.code() << 3));
        }
        Instr::MovDiPtrReg16 { src } => {
            // `mov word ptr [di],<reg16>` → 89 (mod=00 reg=<src>
            // r/m=101). r/m=101 = [DI]. Fixture 628.
            out.push(0x89);
            out.push(0b00_000_101 | (src.code() << 3));
        }
        Instr::MovSiDispImm { disp, imm } => {
            // `mov word ptr [si+disp8],imm16` → C7 44 dd lo hi.
            // ModR/M 44 = mod=01 /0 r/m=100 ([si+disp8]).
            out.push(0xC7);
            out.push(0x44);
            out.push(*disp as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovSiDispReg16 { disp, reg } => {
            // `mov word ptr [si+disp8],<reg16>` → 89 (mod=01 reg=<r>
            // r/m=100) dd.
            out.push(0x89);
            out.push(0b01_000_100 | (reg.code() << 3));
            out.push(*disp as u8);
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
        Instr::MovReg16FromSiPtr { reg } => {
            // `mov <reg16>, word ptr [si]` → 8B (mod=00 reg=<dst>
            // r/m=100). E.g. `mov bx, [si]` = 8B 1C.
            out.push(0x8B);
            out.push((reg.code() << 3) | 0x04);
        }
        Instr::MovReg16SiDisp { reg, disp } => {
            // `mov <reg16>, word ptr [si+disp8]` → 8B (mod=01
            // reg=<dst> r/m=100) dd.
            out.push(0x8B);
            out.push(0x40 | (reg.code() << 3) | 0x04);
            out.push(*disp as u8);
        }
        Instr::MovReg16FromDiPtr { reg } => {
            // `mov <reg16>, word ptr [di]` → 8B (mod=00 reg=<dst>
            // r/m=101). E.g. `mov ax, [di]` = 8B 05.
            out.push(0x8B);
            out.push((reg.code() << 3) | 0x05);
        }
        Instr::MovReg16DiDisp { reg, disp } => {
            // `mov <reg16>, word ptr [di+disp8]` → 8B (mod=01
            // reg=<dst> r/m=101) dd.
            out.push(0x8B);
            out.push(0x40 | (reg.code() << 3) | 0x05);
            out.push(*disp as u8);
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
        Instr::AddSiPtrAx => {
            // `add word ptr [si],ax` → 01 04. ModR/M 04 = mod=00
            // reg=AX(000) r/m=100=SI. Fixture 838.
            out.push(0x01);
            out.push(0x04);
        }
        Instr::SubSiPtrAx => {
            // `sub word ptr [si],ax` → 29 04. Same ModR/M.
            out.push(0x29);
            out.push(0x04);
        }
        Instr::AndSiPtrAx => {
            // `and word ptr [si],ax` → 21 04.
            out.push(0x21);
            out.push(0x04);
        }
        Instr::OrSiPtrAx => {
            // `or word ptr [si],ax` → 09 04.
            out.push(0x09);
            out.push(0x04);
        }
        Instr::XorSiPtrAx => {
            // `xor word ptr [si],ax` → 31 04.
            out.push(0x31);
            out.push(0x04);
        }
        Instr::AddBxDispAx { disp } => {
            // `add word ptr [bx+disp8],ax` → 01 47 dd. ModR/M `47`
            // = mod=01 reg=AX(000) r/m=111=BX. Fixture 862.
            out.push(0x01);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::SubBxDispAx { disp } => {
            out.push(0x29);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::AndBxDispAx { disp } => {
            out.push(0x21);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::OrBxDispAx { disp } => {
            out.push(0x09);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::XorBxDispAx { disp } => {
            out.push(0x31);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::AddSiDispAx { disp } => {
            // `add word ptr [si+disp8],ax` → 01 44 dd. ModR/M `44`
            // = mod=01 reg=AX(000) r/m=100=SI. Fixture 863.
            out.push(0x01);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::SubSiDispAx { disp } => {
            out.push(0x29);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::AndSiDispAx { disp } => {
            out.push(0x21);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::OrSiDispAx { disp } => {
            out.push(0x09);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::XorSiDispAx { disp } => {
            out.push(0x31);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::AddBxDispImm8 { disp, imm } => {
            // `add word ptr [bx+disp8],imm8sx` → 83 47 dd ii.
            // Group-1 /0 (ADD), mod=01 r/m=111=BX. Fixture 864.
            out.push(0x83);
            out.push(0x47);
            out.push(*disp as u8);
            out.push(*imm as u8);
        }
        Instr::SubBxDispImm8 { disp, imm } => {
            // `sub word ptr [bx+disp8],imm8sx` → 83 6F dd ii.
            out.push(0x83);
            out.push(0x6F);
            out.push(*disp as u8);
            out.push(*imm as u8);
        }
        Instr::MovAlBxDisp { disp } => {
            // `mov al,byte ptr [bx+disp8]` → 8A 47 dd. Fixture 865.
            out.push(0x8A);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::MovBxDispAl { disp } => {
            // `mov byte ptr [bx+disp8],al` → 88 47 dd. Sibling.
            out.push(0x88);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::AndBxDispAl { disp } => {
            // `and byte ptr [bx+disp8],al` → 20 47 dd. Fixture 870.
            out.push(0x20);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::OrBxDispAl { disp } => {
            // `or byte ptr [bx+disp8],al` → 08 47 dd.
            out.push(0x08);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::XorBxDispAl { disp } => {
            // `xor byte ptr [bx+disp8],al` → 30 47 dd.
            out.push(0x30);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::AndBxDispImm16 { disp, imm } => {
            // `and word ptr [bx+disp8],imm16` → 81 67 dd lo hi.
            // Fixture 875.
            out.push(0x81);
            out.push(0x67);
            out.push(*disp as u8);
            out.push((*imm & 0xFF) as u8);
            out.push((*imm >> 8) as u8);
        }
        Instr::OrBxDispImm16 { disp, imm } => {
            // `or word ptr [bx+disp8],imm16` → 81 4F dd lo hi.
            out.push(0x81);
            out.push(0x4F);
            out.push(*disp as u8);
            out.push((*imm & 0xFF) as u8);
            out.push((*imm >> 8) as u8);
        }
        Instr::XorBxDispImm16 { disp, imm } => {
            // `xor word ptr [bx+disp8],imm16` → 81 77 dd lo hi.
            out.push(0x81);
            out.push(0x77);
            out.push(*disp as u8);
            out.push((*imm & 0xFF) as u8);
            out.push((*imm >> 8) as u8);
        }
        Instr::AddBxPtrAx => {
            // `add word ptr [bx],ax` → 01 07. ModR/M 07 = mod=00
            // reg=AX(000) r/m=111=BX. Fixture 879.
            out.push(0x01);
            out.push(0x07);
        }
        Instr::SubBxPtrAx => {
            out.push(0x29);
            out.push(0x07);
        }
        Instr::AndBxPtrAx => {
            out.push(0x21);
            out.push(0x07);
        }
        Instr::OrBxPtrAx => {
            out.push(0x09);
            out.push(0x07);
        }
        Instr::XorBxPtrAx => {
            out.push(0x31);
            out.push(0x07);
        }
        Instr::IncBxDisp { disp } => {
            // `inc word ptr [bx+disp8]` → FF 47 dd. Fixture 880.
            out.push(0xFF);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::DecBxDisp { disp } => {
            // `dec word ptr [bx+disp8]` → FF 4F dd.
            out.push(0xFF);
            out.push(0x4F);
            out.push(*disp as u8);
        }
        Instr::IncBxDispByte { disp } => {
            // `inc byte ptr [bx+disp8]` → FE 47 dd. Fixture 886.
            out.push(0xFE);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::DecBxDispByte { disp } => {
            // `dec byte ptr [bx+disp8]` → FE 4F dd.
            out.push(0xFE);
            out.push(0x4F);
            out.push(*disp as u8);
        }
        Instr::CmpBxDispImm8 { disp, imm } => {
            // `cmp word ptr [bx+disp8],imm8sx` → 83 7F dd ii.
            // Fixture 889.
            out.push(0x83);
            out.push(0x7F);
            out.push(*disp as u8);
            out.push(*imm as u8);
        }
        Instr::ShlBxDispImm1 { disp } => {
            // `shl word ptr [bx+disp8],1` → D1 67 dd. Fixture 878.
            out.push(0xD1);
            out.push(0x67);
            out.push(*disp as u8);
        }
        Instr::SarBxDispImm1 { disp } => {
            // `sar word ptr [bx+disp8],1` → D1 7F dd.
            out.push(0xD1);
            out.push(0x7F);
            out.push(*disp as u8);
        }
        Instr::ShrBxDispImm1 { disp } => {
            // `shr word ptr [bx+disp8],1` → D1 6F dd.
            out.push(0xD1);
            out.push(0x6F);
            out.push(*disp as u8);
        }
        Instr::ShlBxDispCl { disp } => {
            // `shl word ptr [bx+disp8],cl` → D3 67 dd. Fixture 882.
            out.push(0xD3);
            out.push(0x67);
            out.push(*disp as u8);
        }
        Instr::SarBxDispCl { disp } => {
            // `sar word ptr [bx+disp8],cl` → D3 7F dd.
            out.push(0xD3);
            out.push(0x7F);
            out.push(*disp as u8);
        }
        Instr::ShrBxDispCl { disp } => {
            // `shr word ptr [bx+disp8],cl` → D3 6F dd.
            out.push(0xD3);
            out.push(0x6F);
            out.push(*disp as u8);
        }
        Instr::MovAxBxDisp { disp } => {
            // `mov ax,word ptr [bx+disp8]` → 8B 47 dd. Fixture 883.
            out.push(0x8B);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::MovBxDispAx { disp } => {
            // `mov word ptr [bx+disp8],ax` → 89 47 dd. Store sibling.
            out.push(0x89);
            out.push(0x47);
            out.push(*disp as u8);
        }
        Instr::MovBxDispDx { disp } => {
            // `mov word ptr [bx+disp8],dx` → 89 57 dd. Fixture 884.
            out.push(0x89);
            out.push(0x57);
            out.push(*disp as u8);
        }
        Instr::MovDxBxDisp { disp } => {
            // `mov dx,word ptr [bx+disp8]` → 8B 57 dd. Fixture 904.
            out.push(0x8B);
            out.push(0x57);
            out.push(*disp as u8);
        }
        Instr::MovBxDispImm { disp, imm } => {
            // `mov word ptr [bx+disp8],imm16` → C7 47 dd lo hi.
            // Group with /0, mod=01 r/m=111=BX+disp8. Fixture 897.
            out.push(0xC7);
            out.push(0x47);
            out.push(*disp as u8);
            out.push((*imm & 0xFF) as u8);
            out.push((*imm >> 8) as u8);
        }
        Instr::AdcBxDispImm8 { disp, imm } => {
            // `adc word ptr [bx+disp8],imm8sx` → 83 57 dd ii.
            // Group-1 /2 (ADC). Fixture 901.
            out.push(0x83);
            out.push(0x57);
            out.push(*disp as u8);
            out.push(*imm as u8);
        }
        Instr::SbbBxDispImm8 { disp, imm } => {
            // `sbb word ptr [bx+disp8],imm8sx` → 83 5F dd ii.
            // Group-1 /3 (SBB).
            out.push(0x83);
            out.push(0x5F);
            out.push(*disp as u8);
            out.push(*imm as u8);
        }
        Instr::PushBxDisp { disp } => {
            // `push word ptr [bx+disp8]` → FF 77 dd. ModR/M `77` =
            // mod=01 reg=/6(PUSH) r/m=111=BX. Fixture 893.
            out.push(0xFF);
            out.push(0x77);
            out.push(*disp as u8);
        }
        Instr::AddAlBpRel { offset } => {
            // `add al, byte ptr [bp+disp]` → 02 /AL [bp+disp]. ADD
            // r8, r/m8. Fixture 847.
            out.push(0x02);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::SubAlBpRel { offset } => {
            out.push(0x2A);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::AndAlBpRel { offset } => {
            out.push(0x22);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::OrAlBpRel { offset } => {
            out.push(0x0A);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::XorAlBpRel { offset } => {
            out.push(0x32);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::ShlSiPtrCl => {
            // `shl word ptr [si],cl` → D3 24. Grp2 /4(SHL) r/m=100. Fixture 840.
            out.push(0xD3);
            out.push(0x24);
        }
        Instr::SarSiPtrCl => {
            // `sar word ptr [si],cl` → D3 3C. Grp2 /7(SAR) r/m=100.
            out.push(0xD3);
            out.push(0x3C);
        }
        Instr::ShrSiPtrCl => {
            // `shr word ptr [si],cl` → D3 2C. Grp2 /5(SHR) r/m=100.
            out.push(0xD3);
            out.push(0x2C);
        }
        Instr::AdcSiDispAx { disp } => {
            // `adc word ptr [si+disp8],ax` → 11 44 dd. ModR/M
            // 44 = mod=01 reg=AX(000) r/m=100=SI with disp8.
            out.push(0x11);
            out.push(0x44);
            out.push(*disp as u8);
        }
        Instr::AdcSiDispDx { disp } => {
            // `adc word ptr [si+disp8],dx` → 11 54 dd. ModR/M
            // 54 = mod=01 reg=DX(010) r/m=100=SI. Fixture 849.
            out.push(0x11);
            out.push(0x54);
            out.push(*disp as u8);
        }
        Instr::SbbSiDispDx { disp } => {
            // `sbb word ptr [si+disp8],dx` → 19 54 dd. Sub-with-
            // borrow sibling.
            out.push(0x19);
            out.push(0x54);
            out.push(*disp as u8);
        }
        Instr::SubSiPtrImm8 { imm } => {
            // `sub word ptr [si],imm8sx` → 83 2C ii.
            // ModR/M 2C = mod=00 /5(SUB) r/m=100.
            out.push(0x83);
            out.push(0x2C);
            out.push(*imm as u8);
        }
        Instr::AddSiPtrReg8 { src } => {
            // `add byte ptr [si], <reg8>` → 00 (mod=00 reg=<r>
            // r/m=100). Fixture 713 (`add [si], al` = 00 04).
            out.push(0x00);
            out.push(0b00_000_100 | (src.code() << 3));
        }
        Instr::SubSiPtrReg8 { src } => {
            // `sub byte ptr [si], <reg8>` → 28 (mod=00 reg=<r>
            // r/m=100).
            out.push(0x28);
            out.push(0b00_000_100 | (src.code() << 3));
        }
        Instr::IncSiPtrByte => {
            // `inc byte ptr [si]` → FE 04. Grp4 /0 r/m8 with
            // mod=00 r/m=100. Fixture 714.
            out.push(0xFE);
            out.push(0x04);
        }
        Instr::DecSiPtrByte => {
            // `dec byte ptr [si]` → FE 0C.
            out.push(0xFE);
            out.push(0x0C);
        }
        Instr::IncSiPtrWord => {
            // `inc word ptr [si]` → FF 04.
            out.push(0xFF);
            out.push(0x04);
        }
        Instr::DecSiPtrWord => {
            // `dec word ptr [si]` → FF 0C.
            out.push(0xFF);
            out.push(0x0C);
        }
        Instr::AndSiPtrByteImm8 { imm } => {
            // `and byte ptr [si],imm8` → 80 24 ii.
            // ModR/M 24 = mod=00 /4(AND) r/m=100. Fixture 712.
            out.push(0x80);
            out.push(0x24);
            out.push(*imm);
        }
        Instr::OrSiPtrByteImm8 { imm } => {
            // `or byte ptr [si],imm8` → 80 0C ii.
            out.push(0x80);
            out.push(0x0C);
            out.push(*imm);
        }
        Instr::XorSiPtrByteImm8 { imm } => {
            // `xor byte ptr [si],imm8` → 80 34 ii.
            out.push(0x80);
            out.push(0x34);
            out.push(*imm);
        }
        Instr::AndBpRelByteImm8 { offset, imm } => {
            // `and byte ptr [bp+disp], imm8` → 80 /4 [bp+disp] ii.
            // Grp1 r/m8 imm8. Fixture 720.
            out.push(0x80);
            emit_bp_rel_modrm(4, *offset, out);
            out.push(*imm);
        }
        Instr::OrBpRelByteImm8 { offset, imm } => {
            out.push(0x80);
            emit_bp_rel_modrm(1, *offset, out);
            out.push(*imm);
        }
        Instr::XorBpRelByteImm8 { offset, imm } => {
            out.push(0x80);
            emit_bp_rel_modrm(6, *offset, out);
            out.push(*imm);
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
            // `add word ptr [bp+disp], imm8sx` → 83 /0 [bp+disp] ii.
            out.push(0x83);
            emit_bp_rel_modrm(0, *offset, out);
            out.push(*imm as u8);
        }
        Instr::AdcBpRelImm8 { offset, imm } => {
            out.push(0x83);
            emit_bp_rel_modrm(2, *offset, out);
            out.push(*imm as u8);
        }
        Instr::SubBpRelImm8 { offset, imm } => {
            out.push(0x83);
            emit_bp_rel_modrm(5, *offset, out);
            out.push(*imm as u8);
        }
        Instr::SbbBpRelImm8 { offset, imm } => {
            out.push(0x83);
            emit_bp_rel_modrm(3, *offset, out);
            out.push(*imm as u8);
        }
        Instr::AndBpRelImm16 { offset, imm } => {
            out.push(0x81);
            emit_bp_rel_modrm(4, *offset, out);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::OrBpRelImm16 { offset, imm } => {
            out.push(0x81);
            emit_bp_rel_modrm(1, *offset, out);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::XorBpRelImm16 { offset, imm } => {
            out.push(0x81);
            emit_bp_rel_modrm(6, *offset, out);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovAxFromSiPtr => {
            // `mov ax,word ptr [si]` → 8B 04. ModR/M 04 = mod=00
            // reg=AX r/m=100 ([si]).
            out.push(0x8B);
            out.push(0x04);
        }
        Instr::MovBxPtrAl => {
            // `mov byte ptr [bx],al` → 88 07. ModR/M 07 = mod=00
            // reg=AL r/m=BX.
            out.push(0x88);
            out.push(0x07);
        }
        Instr::MovBxPtrAx => {
            // `mov word ptr [bx],ax` → 89 07. Same ModR/M, word opcode.
            out.push(0x89);
            out.push(0x07);
        }
        Instr::MovBxPtrReg16 { reg } => {
            // `mov word ptr [bx],<reg16>` → 89 (mod=00 reg=<r>
            // r/m=111).
            out.push(0x89);
            out.push(0b00_000_111 | (reg.code() << 3));
        }
        Instr::MovBxPtrImm { imm } => {
            // `mov word ptr [bx],imm16` → C7 07 lo hi. ModR/M 07 =
            // mod=00 /0 r/m=111 ([bx]).
            out.push(0xC7);
            out.push(0x07);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::MovBxPtrImm8 { imm } => {
            // `mov byte ptr [bx],imm8` → C6 07 ii.
            out.push(0xC6); out.push(0x07); out.push(*imm);
        }
        Instr::MovDiPtrImm { imm } => {
            // `mov word ptr [di],imm16` → C7 05 lo hi. ModR/M 05 =
            // mod=00 /0 r/m=101 ([di]).
            out.push(0xC7); out.push(0x05);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::AddSiPtrImm16 { imm } => {
            // `add word ptr [si],imm16` → 81 04 lo hi. Grp1 /0=ADD,
            // ModR/M 04 = mod=00 r/m=100 ([si]).
            out.push(0x81); out.push(0x04);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Instr::XorDiPtrReg16 { reg } => {
            // `xor word ptr [di],<reg16>` → 31 (mod=00 reg=<r> r/m=101).
            out.push(0x31);
            out.push(0b00_000_101 | (reg.code() << 3));
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
        Instr::ShlReg8One { reg } => {
            // `shl r8,1` → D0 (mod=11 /4 r/m=<reg-code>). 8-bit
            // sibling of `ShlReg16One`. Fixture 535.
            out.push(0xD0);
            out.push(0b11_100_000 | reg.code());
        }
        Instr::SarReg8One { reg } => {
            // `sar r8,1` → D0 (mod=11 /7 r/m=<reg-code>).
            out.push(0xD0);
            out.push(0b11_111_000 | reg.code());
        }
        Instr::ShrReg8One { reg } => {
            // `shr r8,1` → D0 (mod=11 /5 r/m=<reg-code>).
            out.push(0xD0);
            out.push(0b11_101_000 | reg.code());
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
            // `mov word ptr [bp+disp], offset _f` → C7 /0 [bp+disp]
            // lo hi. The imm bytes carry the symbol's segment-
            // relative offset (TLINK patches via the FIXUPP). FIXUPP
            // frame is F5 (target's own segment) because the target
            // is in _TEXT, which is not in any group.
            let sym_loc = symbols.get(symbol).ok_or_else(|| {
                AsmError::new(0, format!("symbol `{symbol}` not defined in any segment"))
            })?;
            let target_seg_idx =
                u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
            out.push(0xC7);
            emit_bp_rel_modrm(0, *offset, out);
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
            // `call word ptr [bp+disp]` → FF /2 [bp+disp]. Grp5 /2 =
            // CALL near r/m16.
            out.push(0xFF);
            emit_bp_rel_modrm(2, *offset, out);
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
        Instr::FldDwordBpRel { offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x9B);
            out.push(0xD9);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::FstpDwordBpRel { offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x9B);
            out.push(0xD9);
            emit_bp_rel_modrm(3, *offset, out);
        }
        Instr::FldQwordBpRel { offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x9B);
            out.push(0xDD);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::FstpQwordBpRel { offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x9B);
            out.push(0xDD);
            emit_bp_rel_modrm(3, *offset, out);
        }
        Instr::FldDwordGroupSym { group, symbol, offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            emit_group_sym_lea(
                &[0x9B, 0xD9, 0x06], group, symbol, *offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
        }
        Instr::FldQwordGroupSym { group, symbol, offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            emit_group_sym_lea(
                &[0x9B, 0xDD, 0x06], group, symbol, *offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
        }
        Instr::FstpDwordGroupSym { group, symbol, offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            emit_group_sym_lea(
                &[0x9B, 0xD9, 0x1E], group, symbol, *offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
        }
        Instr::FstpQwordGroupSym { group, symbol, offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            emit_group_sym_lea(
                &[0x9B, 0xDD, 0x1E], group, symbol, *offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
        }
        Instr::FpuArithBpRel { op, width, offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x9B);
            out.push(width.arith_family());
            emit_bp_rel_modrm(op.reg_code(), *offset, out);
        }
        Instr::FpuArithGroupSym { op, width, group, symbol, offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            // ModR/M: mod=00 (no displacement, but r/m=110 means
            // 16-bit direct displacement follows), reg=<op>,
            // r/m=110. Byte = (reg << 3) | 0x06.
            let modrm = (op.reg_code() << 3) | 0x06;
            let opcode_prefix = [0x9B, width.arith_family(), modrm];
            emit_group_sym_lea(
                &opcode_prefix, group, symbol, *offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
        }
        Instr::Fld1 => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.extend_from_slice(&[0x9B, 0xD9, 0xE8]);
        }
        Instr::Fchs => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.extend_from_slice(&[0x9B, 0xD9, 0xE0]);
        }
        Instr::FsubpStack => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.extend_from_slice(&[0x9B, 0xDE, 0xE9]);
        }
        Instr::FildWordBpRel { offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x9B);
            out.push(0xDF);
            emit_bp_rel_modrm(0, *offset, out);
        }
        Instr::FcompBpRel { width, offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x9B);
            out.push(width.arith_family());
            emit_bp_rel_modrm(3, *offset, out);
        }
        Instr::FcompGroupSym { width, group, symbol, offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            // ModR/M `1E` = mod=00, reg=011 (/3 fcomp), r/m=110
            // (disp16 direct memory).
            let opcode_prefix = [0x9B, width.arith_family(), 0x1E];
            emit_group_sym_lea(
                &opcode_prefix, group, symbol, *offset,
                symbols, group_idx, extern_idx, out, fixups,
            )?;
        }
        Instr::FstswWordBpRel { offset } => {
            push_fidrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x9B);
            out.push(0xDD);
            emit_bp_rel_modrm(7, *offset, out);
        }
        Instr::Fwait => {
            // Marker fixup targets the NOP byte (the byte we're
            // about to emit), then `90 9B`. Real TASM tags fwait
            // with `FIWRQQ` rather than `FIDRQQ`.
            push_fiwrqq_fixup(out, extern_idx, fixups)?;
            out.push(0x90);
            out.push(0x9B);
        }
        Instr::Sahf => out.push(0x9E),
    }
    Ok(())
}

/// Push a `SegRelExternFrameTarget` FIXUPP pointing at the *next*
/// byte we're about to emit, targeting the `FIDRQQ` EXTDEF entry.
/// Real TASM emits one of these per 8087 instruction so the linker
/// can rewrite the site for emulation. Errors out if `FIDRQQ` isn't
/// in the EXTDEF table — the top-level `assemble()` injects it
/// whenever any FPU instruction is present.
fn push_fidrqq_fixup(
    out: &Vec<u8>,
    extern_idx: &HashMap<String, u8>,
    fixups: &mut Vec<FixupReq>,
) -> AsmResult<()> {
    push_marker_fixup(out, extern_idx, fixups, "FIDRQQ")
}

/// Like `push_fidrqq_fixup` but targets the `FIWRQQ` marker —
/// used for the standalone `fwait` mnemonic. Real TASM treats
/// fwait as a distinct synchronization request worthy of its own
/// EXTDEF entry; the linker uses it the same way as FIDRQQ.
fn push_fiwrqq_fixup(
    out: &Vec<u8>,
    extern_idx: &HashMap<String, u8>,
    fixups: &mut Vec<FixupReq>,
) -> AsmResult<()> {
    push_marker_fixup(out, extern_idx, fixups, "FIWRQQ")
}

fn push_marker_fixup(
    out: &Vec<u8>,
    extern_idx: &HashMap<String, u8>,
    fixups: &mut Vec<FixupReq>,
    marker: &'static str,
) -> AsmResult<()> {
    let idx = *extern_idx.get(marker).ok_or_else(|| {
        AsmError::new(
            0,
            format!("FPU instruction emitted but `{marker}` marker missing from EXTDEFs"),
        )
    })?;
    let off = u16::try_from(out.len()).map_err(|_| {
        AsmError::new(0, "LEDATA offset exceeds u16 at FPU instruction")
    })?;
    fixups.push(FixupReq {
        data_offset: off,
        kind: FixupKind::SegRelExternFrameTarget { extdef_idx: idx },
    });
    Ok(())
}

/// Encode an `<op> ax,word ptr [bp+disp]` instruction. The opcode
/// byte varies (03=add, 2B=sub, 23=and, 0B=or, 33=xor, 3B=cmp); the
/// ModR/M byte is always 0x46 (mod=01, reg=000=AX, r/m=110=[bp+disp8]).
fn emit_alu_ax_bp_rel(opcode: u8, offset: i16, out: &mut Vec<u8>) {
    out.push(opcode);
    emit_bp_rel_modrm(0, offset, out);
}

/// Emit a `[bp+disp]` ModR/M byte and its displacement bytes,
/// picking the disp8 form (`mod=01`) when the offset fits in i8 and
/// the disp16 form (`mod=10`) otherwise. The `reg_field` is the
/// 3-bit `reg`/opcode-extension value placed at bits 3..5 of the
/// ModR/M; `r/m` is always 110 (BP). Used by every bp-relative op
/// (load/store/alu against a stack local) so frames > 128 bytes
/// just promote each ref to disp16 instead of crashing.
fn emit_bp_rel_modrm(reg_field: u8, offset: i16, out: &mut Vec<u8>) {
    debug_assert!(reg_field < 8, "reg_field is 3 bits");
    if let Ok(disp) = i8::try_from(offset) {
        out.push(0b01_000_110 | (reg_field << 3));
        out.push(disp as u8);
    } else {
        out.push(0b10_000_110 | (reg_field << 3));
        out.extend_from_slice(&(offset as u16).to_le_bytes());
    }
}

/// Size (in bytes) of the ModR/M + displacement for a `[bp+disp]`
/// reference at this offset — 2 for disp8, 3 for disp16. Used by
/// `instr_size` to compute total instruction length when the disp
/// is offset-dependent.
fn bp_rel_modrm_size(offset: i16) -> usize {
    if i8::try_from(offset).is_ok() { 2 } else { 3 }
}

/// Shared helper for `<op> {ax|al},<form>:<sym>` instructions where
/// the encoding is `<opcode-prefix> <16-bit-symbol-offset>` plus a
/// SegRelGroupTarget FIXUPP. Opcode-prefix length varies by op:
///   1 byte for `mov ax,moffs16` (A1), `mov al,moffs8` (A0), and
///   `mov ax,offset _sym` (B8). 2 bytes for `add ax,r/m16` with
///   disp16-only addressing (03 06).
/// Emit a 2-byte group-relative symbol reference (no opcode
/// prefix). Same FIXUPP shape as `emit_group_sym_lea` but without
/// the leading opcode bytes — used when a symbol's offset appears
/// as an immediate operand following another opcode-encoded
/// relocation (e.g. the source-symbol imm16 in
/// `Instr::MovGroupSymOffsetGroupSym`).
fn emit_group_sym_imm16(
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
    if let Some(sym_loc) = symbols.get(symbol) {
        let target_seg_idx = u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
        let value = sym_loc.offset.wrapping_add(extra_offset as u16);
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
        if extra_offset != 0 {
            return Err(AsmError::new(
                0,
                format!("extern `{symbol}` with `+{extra_offset}` offset not supported"),
            ));
        }
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
