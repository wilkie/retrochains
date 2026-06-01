use super::*;

pub(crate) fn encode_segment(
    seg_idx: usize,
    seg: &Segment,
    symbols: &Symbols,
    group_idx: &HashMap<String, u8>,
    _segment_idx: &HashMap<String, u8>,
    extern_idx: &HashMap<String, u8>,
    expanded: &ExpandedJccs,
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

    for (item_idx, item) in seg.items.iter().enumerate() {
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
                let g_idx = *group_idx.get(group).ok_or_else(|| {
                    AsmError::new(0, format!("dw: group `{group}` not defined"))
                })?;
                // Symbol either lives in this module (PUBDEF/local) or
                // is declared via `extrn` — emit different FIXUPP for
                // each. Fixture 3643 (`int (*fp)(int) = add1;` where
                // `add1` is a prototype-only).
                if let Some(sym_loc) = symbols.get(symbol) {
                    let target_seg_idx =
                        u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
                    let value = sym_loc.offset.wrapping_add(*extra_offset as u16);
                    let imm_start = bytes.len();
                    bytes.extend_from_slice(&value.to_le_bytes());
                    // BCC's frame method depends on whether the target
                    // segment is in the same group (DGROUP) as the dw
                    // site. `_TEXT` (segment 0) is a separate group,
                    // so a dw in _DATA referencing _TEXT uses Frame=
                    // TARGET (0x54, no frame datum). Otherwise GROUP
                    // (0x14). Fixture 192 (`char *p = "hi"`, _DATA
                    // target → GROUP), fixture 3212 (`int (*ptr)(int)
                    // = target`, _TEXT target → TARGET).
                    let kind = if sym_loc.segment == 0 {
                        FixupKind::SegRelTargetFrameSegment {
                            segment_idx: target_seg_idx,
                        }
                    } else {
                        FixupKind::SegRelGroupTarget {
                            group_idx: g_idx,
                            segment_idx: target_seg_idx,
                        }
                    };
                    fixups.push(FixupReq {
                        data_offset: u16::try_from(imm_start).expect("offset fits"),
                        kind,
                    });
                } else if let Some(&ext_idx) = extern_idx.get(symbol) {
                    if *extra_offset != 0 {
                        return Err(AsmError::new(
                            0,
                            format!("dw: extern `{symbol}` with `+{extra_offset}` offset not supported"),
                        ));
                    }
                    let imm_start = bytes.len();
                    bytes.extend_from_slice(&0u16.to_le_bytes());
                    // `dw <group>:<extern_sym>` in _DATA uses an F5
                    // (target-frame) FIXUP per BCC. Fixture 3643
                    // (`int (*fp)(int) = add1;` where add1 is a
                    // prototype-only function).
                    let _ = g_idx;
                    fixups.push(FixupReq {
                        data_offset: u16::try_from(imm_start).expect("offset fits"),
                        kind: FixupKind::SegRelExternFrameTargetF5 {
                            extdef_idx: ext_idx,
                        },
                    });
                } else {
                    return Err(AsmError::new(
                        0,
                        format!("dw: symbol `{symbol}` not defined"),
                    ));
                }
            }
            SegItem::DdGroupSym { group, symbol, extra_offset } => {
                if sealed_bytes {
                    return Err(AsmError::new(
                        0,
                        format!("segment {}: `dd` after padding not supported", seg.name),
                    ));
                }
                sealed_pad = true;
                let g_idx = *group_idx.get(group).ok_or_else(|| {
                    AsmError::new(0, format!("dd: group `{group}` not defined"))
                })?;
                let sym_loc = symbols.get(symbol).ok_or_else(|| {
                    AsmError::new(0, format!("dd: symbol `{symbol}` not defined"))
                })?;
                let target_seg_idx =
                    u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
                let value = sym_loc.offset.wrapping_add(*extra_offset as u16);
                let imm_start = bytes.len();
                // Offset half (bytes 0-1) carries the pre-resolved
                // value; the linker adds the group-frame-relative
                // offset of `<symbol>` on top. Segment half (bytes
                // 2-3) is zero; the linker writes the group's
                // paragraph there. Fixtures 3760 / 3761.
                bytes.extend_from_slice(&value.to_le_bytes());
                bytes.extend_from_slice(&0u16.to_le_bytes());
                fixups.push(FixupReq {
                    data_offset: u16::try_from(imm_start).expect("offset fits"),
                    kind: FixupKind::FarPtrGroupTarget {
                        group_idx: g_idx,
                        segment_idx: target_seg_idx,
                    },
                });
            }
            SegItem::DdSym { symbol, extra_offset } => {
                if sealed_bytes {
                    return Err(AsmError::new(
                        0,
                        format!("segment {}: `dd` after padding not supported", seg.name),
                    ));
                }
                sealed_pad = true;
                let sym_loc = symbols.get(symbol).ok_or_else(|| {
                    AsmError::new(0, format!("dd: symbol `{symbol}` not defined"))
                })?;
                let target_seg_idx =
                    u8::try_from(sym_loc.segment + 1).expect("target seg idx fits");
                let value = sym_loc.offset.wrapping_add(*extra_offset as u16);
                let imm_start = bytes.len();
                bytes.extend_from_slice(&value.to_le_bytes());
                bytes.extend_from_slice(&0u16.to_le_bytes());
                fixups.push(FixupReq {
                    data_offset: u16::try_from(imm_start).expect("offset fits"),
                    kind: FixupKind::FarPtrSegmentTarget {
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
                let jcc_expanded = matches!(instr, Instr::JmpCondShort { .. })
                    && expanded.contains(&(seg_idx, item_idx));
                emit_instr(
                    seg_idx,
                    instr,
                    symbols,
                    group_idx,
                    extern_idx,
                    _segment_idx,
                    &mut bytes,
                    &mut fixups,
                    jcc_expanded,
                )?;
            }
        }
    }

    let size = u32::try_from(bytes.len()).unwrap() + pad;
    Ok(EncodedSeg { size, bytes, fixups })
}
