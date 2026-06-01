use super::*;

/// Push a `SegRelExternFrameTarget` FIXUPP pointing at the *next*
/// byte we're about to emit, targeting the `FIDRQQ` EXTDEF entry.
/// Real TASM emits one of these per 8087 instruction so the linker
/// can rewrite the site for emulation. Errors out if `FIDRQQ` isn't
/// in the EXTDEF table — the top-level `assemble()` injects it
/// whenever any FPU instruction is present.
pub(crate) fn push_fidrqq_fixup(
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
pub(crate) fn push_fiwrqq_fixup(
    out: &Vec<u8>,
    extern_idx: &HashMap<String, u8>,
    fixups: &mut Vec<FixupReq>,
) -> AsmResult<()> {
    push_marker_fixup(out, extern_idx, fixups, "FIWRQQ")
}
pub(crate) fn push_marker_fixup(
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
pub(crate) fn emit_alu_ax_bp_rel(opcode: u8, offset: i16, out: &mut Vec<u8>) {
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
pub(crate) fn emit_bp_rel_modrm(reg_field: u8, offset: i16, out: &mut Vec<u8>) {
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
pub(crate) fn bp_rel_modrm_size(offset: i16) -> usize {
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
pub(crate) fn emit_group_sym_imm16(
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
pub(crate) fn emit_group_sym_lea(
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
