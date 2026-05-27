//! Emit OMF records for a parsed [`Module`]. The record order and exact
//! payloads must match what Borland's TASM 2.0 produces, since byte-
//! exactness against captured fixtures is the only test we trust.

use obj::ObjBuilder;

use crate::encode::{encode_module, EncodedSeg};
use crate::ir::{AsmResult, FixupKind, FixupReq, Module, SegAlign, SegCombine};

/// The translator string TASM 2.0 emits as the COMENT-class-0x00 record.
/// This is the most distinctive single byte sequence in any BCC 2.0
/// OBJ — see `specs/formats/OMF.md` §COMENT.
const TRANSLATOR_STRING: &[u8] = b"TC86 Borland Turbo C++ 2.0";

/// The COMENT class 0xEA payload TASM emits for the small memory model.
/// The 0x01 is plausibly a model-id (small); the 0x09 is unexplained.
/// Will need broadening when we capture non-small-model fixtures.
const MEMORY_MODEL_MARKER: &[u8] = &[0x01, 0x09];

pub fn emit(module: &Module) -> AsmResult<Vec<u8>> {
    let encoded = encode_module(module)?;

    let mut b = ObjBuilder::new();

    // 1. THEADR.
    b.write_theadr(&module.source_name);

    // 2. COMENT class 0x00 — translator identifier.
    let mut tr = Vec::with_capacity(3 + TRANSLATOR_STRING.len());
    tr.push(0x00);
    tr.push(0x00);
    tr.push(u8::try_from(TRANSLATOR_STRING.len()).expect("translator string fits"));
    tr.extend_from_slice(TRANSLATOR_STRING);
    b.write_coment(&tr);

    // 3..N. Pass-through COMENTs from `?debug C HEX` directives.
    for blob in &module.debug_comments {
        let mut payload = Vec::with_capacity(1 + blob.len());
        payload.push(0x00);
        payload.extend_from_slice(blob);
        b.write_coment(&payload);
    }

    // N+1. COMENT class 0xEA — memory-model marker.
    let mut model_coment = Vec::with_capacity(2 + MEMORY_MODEL_MARKER.len());
    model_coment.push(0x00);
    model_coment.push(0xEA);
    model_coment.extend_from_slice(MEMORY_MODEL_MARKER);
    b.write_coment(&model_coment);

    // LNAMES.
    let (lnames, seg_name_idx, seg_class_idx, group_name_idx) = build_lnames(module);
    let lnames_refs: Vec<&str> = lnames.iter().map(String::as_str).collect();
    b.write_lnames(&lnames_refs);

    // SEGDEFs — use the encoded size (which includes any padding).
    for (i, seg) in module.segments.iter().enumerate() {
        let acbp = pack_acbp(seg.align, seg.combine);
        let len = u16::try_from(encoded.segments[i].size)
            .expect("segment fits in u16");
        b.write_segdef16(acbp, len, seg_name_idx[i], seg_class_idx[i], /*overlay*/ 1);
    }

    // GRPDEFs.
    for (gi, group) in module.groups.iter().enumerate() {
        let seg_idxs: Vec<u8> = group
            .segments
            .iter()
            .map(|n| {
                let pos = module
                    .segments
                    .iter()
                    .position(|s| s.name == *n)
                    .expect("group segment must exist");
                u8::try_from(pos + 1).expect("segment idx fits")
            })
            .collect();
        b.write_grpdef(group_name_idx[gi], &seg_idxs);
    }

    // EXTDEFs — emit only if the module references externs.
    if !module.externs.is_empty() {
        let refs: Vec<&str> = module.externs.iter().map(String::as_str).collect();
        b.write_extdef(&refs);
    }

    // PUBDEFs in source order. The base_group_idx is the index of
    // whichever group physically contains the symbol's home segment
    // (so a global in `_BSS` reads through DGROUP), or 0 for symbols
    // in standalone segments like `_TEXT`.
    for name in &module.publics {
        let loc = encoded
            .symbols
            .get(name.as_str())
            .unwrap_or_else(|| panic!("public symbol `{name}` not defined in any segment"));
        let seg_idx_1based = u8::try_from(loc.segment + 1).expect("seg idx fits");
        let seg_name = module.segments[loc.segment].name.as_str();
        let group_idx = group_containing_segment(module, seg_name).unwrap_or(0);
        b.write_pubdef16(group_idx, seg_idx_1based, name, loc.offset, 0);
    }

    // LEDATA + FIXUPP per non-empty segment.
    for (i, eseg) in encoded.segments.iter().enumerate() {
        if eseg.bytes.is_empty() {
            continue;
        }
        let seg_idx_1based = u8::try_from(i + 1).expect("seg idx fits");
        b.write_ledata16(seg_idx_1based, 0, &eseg.bytes);
        if !eseg.fixups.is_empty() {
            let payload = serialize_fixupp(eseg);
            b.write_fixupp(&payload);
        }
    }

    // Trailing COMENT class 0xE8.
    let mtime_bytes = extract_mtime_from_debug_comments(&module.debug_comments);
    let mut tail = Vec::with_capacity(4 + module.source_name.len() + 4);
    tail.push(0x00);
    tail.push(0xE8);
    tail.push(0x00);
    tail.push(u8::try_from(module.source_name.len()).expect("name fits"));
    tail.extend_from_slice(module.source_name.as_bytes());
    tail.extend_from_slice(&mtime_bytes);
    b.write_coment(&tail);

    // MODEND.
    b.write_modend16_no_entry();

    Ok(b.into_bytes())
}

fn serialize_fixupp(seg: &EncodedSeg) -> Vec<u8> {
    // TASM emits fixups in LIFO order — the last fixup recorded
    // during instruction encoding becomes the first FIXUP subrecord
    // in the FIXUPP record. Fixture 108 disambiguates: BCC encodes
    // the string-pointer load (offset 8) before the printf call
    // (offset 12), but the captured FIXUPP has the call (offset 12)
    // first, then the string (offset 8). The simplest explanation is
    // a stack-based collection inside TASM.
    let mut payload = Vec::new();
    for fx in seg.fixups.iter().rev() {
        serialize_fixup(fx, &mut payload);
    }
    payload
}

fn serialize_fixup(fx: &FixupReq, out: &mut Vec<u8>) {
    // Locat (2 bytes):
    //   byte 0: 1 M L L L L D D     bit 7 = 1 (FIXUP marker)
    //   byte 1: low 8 bits of data offset
    // Fix Data (1 byte): F=0 frame_method=NNN T=0 P=1 tgt_low=NN
    // Then frame datum (if any) and target datum (if any).
    let off = fx.data_offset;
    let hi2 = (off >> 8) & 0b11;
    let lo8 = (off & 0xFF) as u8;
    match fx.kind {
        FixupKind::SegRelGroupTarget { group_idx, segment_idx } => {
            // M=1 segment-relative, Location=1 (16-bit offset)
            //   Locat byte 0 = 1 1 0001 dd = 0xC4 | hi2
            out.push(0xC4 | hi2 as u8);
            out.push(lo8);
            // Fix Data: F=0, frame=001 (GRPDEF), T=0, P=1, tgt_low=00
            // → target method = 4 (SEGDEF no disp). Byte: 0001 0100 = 0x14.
            out.push(0x14);
            out.push(group_idx);
            out.push(segment_idx);
        }
        FixupKind::SelfRelExtern { extdef_idx } => {
            // M=0 self-relative, Location=1 (16-bit offset)
            //   Locat byte 0 = 1 0 0001 dd = 0x84 | hi2
            out.push(0x84 | hi2 as u8);
            out.push(lo8);
            // Fix Data: F=0, frame=101 (target frame), T=0, P=1,
            // tgt_low=10 → target method = 6 (EXTDEF no disp).
            // Byte: 0101 0110 = 0x56.
            out.push(0x56);
            out.push(extdef_idx);
        }
        FixupKind::SegRelTargetFrameSegment { segment_idx } => {
            // M=1 segment-relative, Location=1 (16-bit offset)
            //   Locat byte 0 = 1 1 0001 dd = 0xC4 | hi2
            out.push(0xC4 | hi2 as u8);
            out.push(lo8);
            // Fix Data: F=0, frame=101 (target frame, no datum),
            // T=0, P=1, tgt_low=00 → target method = 4 (SEGDEF no disp).
            // Byte: 0101 0100 = 0x54.
            out.push(0x54);
            out.push(segment_idx);
        }
        FixupKind::SegRelGroupExtern { group_idx, extdef_idx } => {
            // M=1 segment-relative, Location=1 (16-bit offset)
            //   Locat byte 0 = 0xC4 | hi2
            out.push(0xC4 | hi2 as u8);
            out.push(lo8);
            // Fix Data: F=0, frame=001 (GRPDEF), T=0, P=1, tgt_low=10
            // → target method = 2 (EXTDEF no disp). Byte: 0001 0110 = 0x16.
            out.push(0x16);
            out.push(group_idx);
            out.push(extdef_idx);
        }
        FixupKind::SegRelExternFrameTarget { extdef_idx } => {
            // M=1 segment-relative, Location=1 (16-bit offset)
            //   Locat byte 0 = 0xC4 | hi2
            out.push(0xC4 | hi2 as u8);
            out.push(lo8);
            // Fix Data: F=0, frame=100 (F4 = target's segment, no
            // frame datum), T=0, P=1, tgt_low=10 → target method = 6
            // (EXTDEF no disp). Byte: 0100 0110 = 0x46.
            out.push(0x46);
            out.push(extdef_idx);
        }
        FixupKind::SegRelExternFrameTargetF5 { extdef_idx } => {
            // Same as SegRelExternFrameTarget but with Frame=5 (F5
            // TARGET) instead of F4. Used for runtime-helper data
            // refs where the target's segment isn't known at compile
            // time. Fix Data byte: 0101 0110 = 0x56. Fixtures 2129,
            // 3643.
            out.push(0xC4 | hi2 as u8);
            out.push(lo8);
            out.push(0x56);
            out.push(extdef_idx);
        }
        FixupKind::SegBaseGroupTarget { group_idx, segment_idx } => {
            // M=1, Location=2 (16-bit base / paragraph value)
            //   Locat byte 0 = 1 1 0010 dd = 0xC8 | hi2
            out.push(0xC8 | hi2 as u8);
            out.push(lo8);
            // Fix Data: F=0 frame=001 (GRPDEF), T=0 P=1, tgt_low=00
            // → target method T4 (SEGDEF no disp). Byte: 0001 0100 = 0x14.
            out.push(0x14);
            out.push(group_idx);
            out.push(segment_idx);
        }
    }
}

/// Build the LNAMES list and the 1-based indices each segment/group
/// uses to reference it. BCC's invariant: first entry is empty (the
/// "no overlay" sentinel), then for each declared segment the (name,
/// class) pair appears in declaration order, then group names. Names
/// dedupe globally.
fn build_lnames(module: &Module) -> (Vec<String>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut names: Vec<String> = vec![String::new()];
    let intern = |n: &str, names: &mut Vec<String>| -> u8 {
        if let Some(i) = names.iter().position(|x| x == n) {
            return u8::try_from(i + 1).expect("name idx fits");
        }
        names.push(n.to_string());
        u8::try_from(names.len()).expect("name idx fits")
    };
    let mut seg_name = Vec::with_capacity(module.segments.len());
    let mut seg_class = Vec::with_capacity(module.segments.len());
    for seg in &module.segments {
        seg_name.push(intern(&seg.name, &mut names));
        seg_class.push(intern(&seg.class, &mut names));
    }
    let mut group_name = Vec::with_capacity(module.groups.len());
    for g in &module.groups {
        group_name.push(intern(&g.name, &mut names));
    }
    (names, seg_name, seg_class, group_name)
}

/// Find the 1-based GRPDEF index of the first group that contains
/// `seg_name`, or `None` if the segment isn't in any group.
fn group_containing_segment(module: &Module, seg_name: &str) -> Option<u8> {
    for (gi, g) in module.groups.iter().enumerate() {
        if g.segments.iter().any(|s| s == seg_name) {
            return Some(u8::try_from(gi + 1).expect("group idx fits"));
        }
    }
    None
}

fn pack_acbp(align: SegAlign, combine: SegCombine) -> u8 {
    let align_bits: u8 = match align {
        SegAlign::Byte => 0b001,
        SegAlign::Word => 0b010,
    };
    let combine_bits: u8 = match combine {
        SegCombine::Public => 0b010,
    };
    (align_bits << 5) | (combine_bits << 2)
}

fn extract_mtime_from_debug_comments(blobs: &[Vec<u8>]) -> [u8; 4] {
    for blob in blobs {
        if blob.len() >= 5 && blob[0] == 0xE9 {
            return [blob[1], blob[2], blob[3], blob[4]];
        }
    }
    [0; 4]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    const FIXTURE_001_ASM: &str = include_str!("../../../fixtures/001-empty-main/expected/HELLO.ASM");
    const FIXTURE_002_OBJ: &[u8] = include_bytes!("../../../fixtures/002-empty-main-obj/expected/HELLO.OBJ");

    #[test]
    fn fixture_002_byte_exact() {
        let module = parse(FIXTURE_001_ASM).unwrap();
        let bytes = emit(&module).unwrap();
        assert_eq!(bytes, FIXTURE_002_OBJ);
    }
}
