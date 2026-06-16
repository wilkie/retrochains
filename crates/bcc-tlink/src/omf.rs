//! Parse one OMF object module into the in-memory model the linker
//! combines. Reads the records BCC/TASM emit (see `specs/formats/OMF.md`)
//! via the shared `obj::ObjReader`; we only decode the fields the linker
//! needs to lay out segments, resolve symbols, and apply fixups.

use obj::{ObjReader, Record};

/// A segment definition plus the bytes LEDATA placed into it and the
/// fixups attached to those bytes (already resolved to in-segment offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegDef {
    pub name: String,
    pub class: String,
    /// Alignment field from the ACBP byte (bits 7-5): 1=byte, 2=word,
    /// 3=para, 4=page, 5=dword. 0=absolute (unsupported here).
    pub align: u8,
    /// Combine field from the ACBP byte (bits 4-2): 2=public, 5=stack, …
    pub combine: u8,
    pub length: u16,
    /// Initialized image, `length` bytes (zero-filled where no LEDATA wrote).
    pub data: Vec<u8>,
    /// Whether any LEDATA wrote into this segment (distinguishes a BSS/STACK
    /// segment, which contributes only memory size, from an initialized one).
    pub has_data: bool,
    pub fixups: Vec<Fixup>,
}

/// A group (e.g. `DGROUP`) — a name plus the 1-based segment indices it spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpDef {
    pub name: String,
    pub segments: Vec<u8>,
}

/// A public symbol: a name at an offset within a base segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubDef {
    pub name: String,
    /// 1-based SEGDEF index this symbol is measured from. `0` marks an
    /// absolute symbol, whose value is `absolute_frame:offset` (a constant
    /// not tied to any combined segment).
    pub base_segment: u8,
    pub offset: u16,
    /// Frame paragraph for an absolute (`base_segment == 0`) public.
    pub absolute_frame: u16,
}

/// One fixup, normalized: patch `width` bits at `data_offset` within its
/// segment, pointing at the resolved target, framed per `frame`/`target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixup {
    /// 1-based SEGDEF index of the segment whose bytes are patched.
    pub seg: u8,
    /// Byte offset of the fixup within that segment's data.
    pub data_offset: u16,
    /// `true` = segment-relative, `false` = self-relative (the M bit).
    pub seg_relative: bool,
    /// Location type (4-bit): 1 = near 16-bit offset (all we handle so far).
    pub location: u8,
    /// Frame method (3-bit) and its datum (segment/group/extern index, if any).
    pub frame_method: u8,
    pub frame_datum: Option<u8>,
    /// Target method (3-bit, P||low) and its datum.
    pub target_method: u8,
    pub target_datum: Option<u8>,
}

/// The MODEND start address (logical `seg:offset`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 1-based SEGDEF index the entry is measured from.
    pub base_segment: u8,
    pub offset: u16,
}

/// A fully-parsed object module.
#[derive(Debug, Default)]
pub struct Module {
    pub name: String,
    /// 1-based LNAMES (index 0 is an unused placeholder).
    pub lnames: Vec<String>,
    /// 1-based SEGDEFs (index 0 unused).
    pub segdefs: Vec<SegDef>,
    pub grpdefs: Vec<GrpDef>,
    pub pubdefs: Vec<PubDef>,
    /// 1-based EXTDEF names (index 0 unused).
    pub extdefs: Vec<String>,
    pub entry: Option<Entry>,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("OMF framing: {0}")]
    Framing(#[from] obj::ReadError),
    #[error("truncated {record} payload")]
    Truncated { record: &'static str },
    #[error("LEDATA references segment {0}, which has no SEGDEF")]
    BadSegmentIndex(u8),
    #[error("unsupported OMF record {0:#x} in linker input")]
    Unsupported(u8),
}

/// Read a length-prefixed (Pascal) string from the front of `p`, advancing it.
fn take_pstr(p: &mut &[u8]) -> Result<String, ParseError> {
    let (&len, rest) = p.split_first().ok_or(ParseError::Truncated { record: "name" })?;
    let len = usize::from(len);
    if rest.len() < len {
        return Err(ParseError::Truncated { record: "name" });
    }
    let (bytes, rest) = rest.split_at(len);
    *p = rest;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn take_u8(p: &mut &[u8]) -> Result<u8, ParseError> {
    let (&b, rest) = p.split_first().ok_or(ParseError::Truncated { record: "u8" })?;
    *p = rest;
    Ok(b)
}

fn take_u16(p: &mut &[u8]) -> Result<u16, ParseError> {
    if p.len() < 2 {
        return Err(ParseError::Truncated { record: "u16" });
    }
    let v = u16::from(p[0]) | (u16::from(p[1]) << 8);
    *p = &p[2..];
    Ok(v)
}

/// Parse a complete object module from its raw OMF bytes.
///
/// # Errors
/// Returns [`ParseError`] on a framing error, a truncated payload, a
/// dangling segment index, or an OMF record the linker doesn't yet handle.
pub fn parse(bytes: &[u8]) -> Result<Module, ParseError> {
    let mut module = Module {
        // Index 0 placeholders so the 1-based OMF indices map directly.
        lnames: vec![String::new()],
        segdefs: vec![SegDef {
            name: String::new(),
            class: String::new(),
            align: 0,
            combine: 0,
            length: 0,
            data: Vec::new(),
            has_data: false,
            fixups: Vec::new(),
        }],
        extdefs: vec![String::new()],
        ..Module::default()
    };
    // The segment index of the most recent LEDATA, so a following FIXUPP
    // knows which segment (and base offset) its data offsets are relative to.
    let mut last_ledata: Option<(u8, u16)> = None;

    let mut reader = ObjReader::new(bytes);
    while let Some(rec) = reader.next()? {
        match rec.ty {
            obj::THEADR => {
                let mut p = rec.payload;
                module.name = take_pstr(&mut p)?;
            }
            obj::COMENT => { /* comments don't affect linking */ }
            obj::LNAMES => {
                let mut p = rec.payload;
                while !p.is_empty() {
                    module.lnames.push(take_pstr(&mut p)?);
                }
            }
            obj::SEGDEF_16 => parse_segdef(&mut module, rec.payload)?,
            obj::GRPDEF => parse_grpdef(&mut module, rec.payload)?,
            obj::PUBDEF_16 | obj::LPUBDEF_16 => parse_pubdef(&mut module, rec.payload)?,
            obj::EXTDEF | obj::LEXTDEF => parse_extdef(&mut module, rec.payload)?,
            obj::LEDATA_16 => {
                last_ledata = Some(parse_ledata(&mut module, rec.payload)?);
            }
            obj::LIDATA_16 => {
                last_ledata = Some(parse_lidata(&mut module, rec.payload)?);
            }
            obj::FIXUPP_16 => parse_fixupp(&mut module, rec.payload, last_ledata)?,
            obj::MODEND_16 => parse_modend(&mut module, rec.payload)?,
            other => return Err(unsupported(other, &rec)),
        }
    }
    Ok(module)
}

fn unsupported(ty: u8, _rec: &Record<'_>) -> ParseError {
    ParseError::Unsupported(ty)
}

fn name_at(module: &Module, idx: u8) -> String {
    module.lnames.get(usize::from(idx)).cloned().unwrap_or_default()
}

fn parse_segdef(module: &mut Module, payload: &[u8]) -> Result<(), ParseError> {
    let mut p = payload;
    let acbp = take_u8(&mut p)?;
    let align = (acbp >> 5) & 0x7;
    let combine = (acbp >> 2) & 0x7;
    // Alignment 0 (absolute) carries a frame+offset field we don't support.
    if align == 0 {
        return Err(ParseError::Unsupported(obj::SEGDEF_16));
    }
    let length = take_u16(&mut p)?;
    let name_idx = take_u8(&mut p)?;
    let class_idx = take_u8(&mut p)?;
    let _overlay_idx = take_u8(&mut p)?;
    module.segdefs.push(SegDef {
        name: name_at(module, name_idx),
        class: name_at(module, class_idx),
        align,
        combine,
        length,
        data: vec![0u8; usize::from(length)],
        has_data: false,
        fixups: Vec::new(),
    });
    Ok(())
}

fn parse_grpdef(module: &mut Module, payload: &[u8]) -> Result<(), ParseError> {
    let mut p = payload;
    let name_idx = take_u8(&mut p)?;
    let mut segments = Vec::new();
    while !p.is_empty() {
        let marker = take_u8(&mut p)?;
        // 0xFF = "segment index follows"; other group descriptors are unused.
        if marker != 0xFF {
            return Err(ParseError::Unsupported(obj::GRPDEF));
        }
        segments.push(take_u8(&mut p)?);
    }
    module.grpdefs.push(GrpDef { name: name_at(module, name_idx), segments });
    Ok(())
}

fn parse_pubdef(module: &mut Module, payload: &[u8]) -> Result<(), ParseError> {
    let mut p = payload;
    let _base_group = take_u8(&mut p)?;
    let base_segment = take_u8(&mut p)?;
    // An absolute group (base group 0) with base segment 0 carries a 16-bit
    // Base Frame before the symbol list — these are absolute equates
    // (e.g. `__AHSHIFT`, `__AHINCR`).
    let absolute_frame = if base_segment == 0 { take_u16(&mut p)? } else { 0 };
    while !p.is_empty() {
        let name = take_pstr(&mut p)?;
        let offset = take_u16(&mut p)?;
        let _type_idx = take_u8(&mut p)?;
        module.pubdefs.push(PubDef { name, base_segment, offset, absolute_frame });
    }
    Ok(())
}

fn parse_extdef(module: &mut Module, payload: &[u8]) -> Result<(), ParseError> {
    let mut p = payload;
    while !p.is_empty() {
        let name = take_pstr(&mut p)?;
        let _type_idx = take_u8(&mut p)?;
        module.extdefs.push(name);
    }
    Ok(())
}

fn parse_ledata(module: &mut Module, payload: &[u8]) -> Result<(u8, u16), ParseError> {
    let mut p = payload;
    let seg_idx = take_u8(&mut p)?;
    let offset = take_u16(&mut p)?;
    let seg = module
        .segdefs
        .get_mut(usize::from(seg_idx))
        .ok_or(ParseError::BadSegmentIndex(seg_idx))?;
    let start = usize::from(offset);
    let end = start + p.len();
    if end > seg.data.len() {
        seg.data.resize(end, 0);
    }
    seg.data[start..end].copy_from_slice(p);
    seg.has_data = true;
    Ok((seg_idx, offset))
}

/// Expand a LIDATA (iterated data) record into concrete bytes and write them
/// into the target segment, the same way LEDATA does. The iterated-data blocks
/// recursively encode `repeat × content`; we flatten them.
fn parse_lidata(module: &mut Module, payload: &[u8]) -> Result<(u8, u16), ParseError> {
    let mut p = payload;
    let seg_idx = take_u8(&mut p)?;
    let offset = take_u16(&mut p)?;
    let mut expanded = Vec::new();
    while !p.is_empty() {
        expand_block(&mut p, &mut expanded)?;
    }
    let seg = module
        .segdefs
        .get_mut(usize::from(seg_idx))
        .ok_or(ParseError::BadSegmentIndex(seg_idx))?;
    let start = usize::from(offset);
    let end = start + expanded.len();
    if end > seg.data.len() {
        seg.data.resize(end, 0);
    }
    seg.data[start..end].copy_from_slice(&expanded);
    seg.has_data = true;
    Ok((seg_idx, offset))
}

/// Decode one 16-bit iterated-data block, appending its expansion to `out`.
fn expand_block(p: &mut &[u8], out: &mut Vec<u8>) -> Result<(), ParseError> {
    let repeat = take_u16(p)?;
    let block_count = take_u16(p)?;
    // The content produced by one iteration.
    let mut once = Vec::new();
    if block_count == 0 {
        // Leaf: a length-prefixed run of literal bytes.
        let len = usize::from(take_u8(p)?);
        if p.len() < len {
            return Err(ParseError::Truncated { record: "LIDATA" });
        }
        let (bytes, rest) = p.split_at(len);
        once.extend_from_slice(bytes);
        *p = rest;
    } else {
        for _ in 0..block_count {
            expand_block(p, &mut once)?;
        }
    }
    for _ in 0..repeat {
        out.extend_from_slice(&once);
    }
    Ok(())
}

fn parse_fixupp(
    module: &mut Module,
    payload: &[u8],
    last_ledata: Option<(u8, u16)>,
) -> Result<(), ParseError> {
    let (seg_idx, ledata_offset) =
        last_ledata.ok_or(ParseError::Truncated { record: "FIXUPP (no LEDATA)" })?;
    let mut p = payload;
    while !p.is_empty() {
        let locat_hi = take_u8(&mut p)?;
        // THREAD subrecords (bit 7 = 0) are unused by BCC/TASM here.
        if locat_hi & 0x80 == 0 {
            return Err(ParseError::Unsupported(obj::FIXUPP_16));
        }
        let locat_lo = take_u8(&mut p)?;
        let seg_relative = (locat_hi & 0x40) != 0;
        let location = (locat_hi >> 2) & 0x0F;
        let data_record_offset = (u16::from(locat_hi & 0x03) << 8) | u16::from(locat_lo);

        let fix_data = take_u8(&mut p)?;
        let frame_thread = (fix_data & 0x80) != 0;
        let frame_method = (fix_data >> 4) & 0x07;
        let target_thread = (fix_data & 0x08) != 0;
        let p_bit = (fix_data >> 2) & 0x01;
        let target_method = (p_bit << 2) | (fix_data & 0x03);
        if frame_thread || target_thread {
            return Err(ParseError::Unsupported(obj::FIXUPP_16));
        }
        // Frame datum present for methods 0/1/2 (segment/group/extern frame).
        let frame_datum = if frame_method <= 2 { Some(take_u8(&mut p)?) } else { None };
        // Target datum present for methods 0/1/2/4/5/6 (i.e. all but reserved).
        let target_datum = Some(take_u8(&mut p)?);
        // Explicit-displacement target methods (P=0 → low method 0-2) carry a
        // 16-bit displacement. BCC/TASM use the no-displacement forms (T4-T6).
        if p_bit == 0 {
            let _disp = take_u16(&mut p)?;
        }

        let seg = module
            .segdefs
            .get_mut(usize::from(seg_idx))
            .ok_or(ParseError::BadSegmentIndex(seg_idx))?;
        seg.fixups.push(Fixup {
            seg: seg_idx,
            data_offset: ledata_offset + data_record_offset,
            seg_relative,
            location,
            frame_method,
            frame_datum,
            target_method,
            target_datum,
        });
    }
    Ok(())
}

fn parse_modend(module: &mut Module, payload: &[u8]) -> Result<(), ParseError> {
    let mut p = payload;
    let flags = take_u8(&mut p)?;
    // bit 6 = start address present.
    if flags & 0x40 == 0 {
        return Ok(());
    }
    // End Data: a fixup-style frame/target descriptor, then the offset.
    let end_data = take_u8(&mut p)?;
    let frame_method = (end_data >> 4) & 0x07;
    let p_bit = (end_data >> 2) & 0x01;
    let target_method = (p_bit << 2) | (end_data & 0x03);
    if frame_method <= 2 {
        let _frame_datum = take_u8(&mut p)?;
    }
    // Target datum: the SEGDEF/GRPDEF/EXTDEF index the entry is measured from.
    let target_datum = take_u8(&mut p)?;
    let offset = take_u16(&mut p)?;
    // We support the common TASM form: entry relative to a SEGDEF (method 0/4).
    let base_segment = match target_method {
        0 | 4 => target_datum,
        _ => return Err(ParseError::Unsupported(obj::MODEND_16)),
    };
    module.entry = Some(Entry { base_segment, offset });
    Ok(())
}

/// Serialize a [`Module`] back to OMF object bytes — the inverse of [`parse`].
///
/// This is the foundation for *synthetic* objects: build a `Module` in code
/// (no assembler), `emit` it, and feed it to the linker (or archive it with
/// `bcc-tlib`). The round-trip `parse(&emit(&m))` reproduces every field `parse`
/// records (`lnames` is an internal index table, rebuilt here, so it is not
/// expected to match a hand-set value).
///
/// Limitations (sufficient for synthetic test objects, which are small and use
/// the BCC fixup forms): one `LEDATA` per segment, so a segment's initialized
/// data must be ≤ 1024 bytes (the FIXUPP data-record-offset is 10 bits); fixups
/// use the no-displacement target forms (methods 4/5/6), the only ones `parse`
/// preserves; PUBDEF base-group is emitted as 0.
///
/// # Panics
/// Panics if a name (segment/class/group/public) is longer than 255 bytes or
/// the module has more than 255 distinct names or segments — none of which a
/// real or synthetic 16-bit OMF object reaches.
#[must_use]
pub fn emit(module: &Module) -> Vec<u8> {
    let mut b = obj::ObjBuilder::new();
    b.write_theadr(&module.name);

    // Rebuild the LNAMES table: a leading empty name (BCC's convention), then
    // every distinct segment name, class, and group name in first-use order.
    // `name_idx` returns the 1-based index `parse` will see (its index-0
    // placeholder shifts the record's entries up by one).
    let mut names: Vec<String> = vec![String::new()];
    let idx_of = |names: &mut Vec<String>, s: &str| -> u8 {
        if let Some(pos) = names.iter().position(|n| n == s) {
            u8::try_from(pos + 1).expect("LNAMES index fits in u8")
        } else {
            names.push(s.to_string());
            u8::try_from(names.len()).expect("LNAMES count fits in u8")
        }
    };
    let mut seg_name_idx = Vec::new();
    let mut seg_class_idx = Vec::new();
    for seg in module.segdefs.iter().skip(1) {
        seg_name_idx.push(idx_of(&mut names, &seg.name));
        seg_class_idx.push(idx_of(&mut names, &seg.class));
    }
    let grp_name_idx: Vec<u8> =
        module.grpdefs.iter().map(|g| idx_of(&mut names, &g.name)).collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    b.write_lnames(&name_refs);

    // SEGDEFs (rebuild the ACBP byte from align/combine; big/proc bits unused).
    for (i, seg) in module.segdefs.iter().skip(1).enumerate() {
        let acbp = (seg.align << 5) | (seg.combine << 2);
        b.write_segdef16(acbp, seg.length, seg_name_idx[i], seg_class_idx[i], 0);
    }

    // GRPDEFs.
    for (g, grp) in module.grpdefs.iter().enumerate() {
        b.write_grpdef(grp_name_idx[g], &grp.segments);
    }

    // EXTDEFs (one record listing every external, as BCC does).
    if module.extdefs.len() > 1 {
        let refs: Vec<&str> = module.extdefs.iter().skip(1).map(String::as_str).collect();
        b.write_extdef(&refs);
    }

    // PUBDEFs: one record per (base_segment, absolute_frame) run keeps the
    // payload's leading base fields valid; emitting one per symbol is simplest
    // and re-parses identically.
    for pubdef in &module.pubdefs {
        let mut payload = Vec::new();
        payload.push(0u8); // base group
        payload.push(pubdef.base_segment);
        if pubdef.base_segment == 0 {
            payload.extend_from_slice(&pubdef.absolute_frame.to_le_bytes());
        }
        payload.push(u8::try_from(pubdef.name.len()).expect("public name fits in u8"));
        payload.extend_from_slice(pubdef.name.as_bytes());
        payload.extend_from_slice(&pubdef.offset.to_le_bytes());
        payload.push(0); // type index
        b.write_record(obj::PUBDEF_16, &payload);
    }

    // Per-segment LEDATA + FIXUPP. A segment with no initialized data (BSS /
    // STACK) contributes only its declared length, so it gets no LEDATA.
    for (i, seg) in module.segdefs.iter().skip(1).enumerate() {
        let seg_idx = u8::try_from(i + 1).expect("segment index fits in u8");
        if seg.has_data {
            b.write_ledata16(seg_idx, 0, &seg.data);
        }
        if !seg.fixups.is_empty() {
            let mut payload = Vec::new();
            for fx in &seg.fixups {
                let p_bit = (fx.target_method >> 2) & 1;
                let locat_hi = 0x80
                    | (u8::from(fx.seg_relative) << 6)
                    | ((fx.location & 0xf) << 2)
                    | ((fx.data_offset >> 8) & 0x3) as u8;
                payload.push(locat_hi);
                payload.push((fx.data_offset & 0xff) as u8);
                payload.push((fx.frame_method << 4) | (p_bit << 2) | (fx.target_method & 0x3));
                if let Some(fd) = fx.frame_datum {
                    payload.push(fd);
                }
                payload.push(fx.target_datum.unwrap_or(0));
                if p_bit == 0 {
                    payload.extend_from_slice(&0u16.to_le_bytes()); // displacement (not retained)
                }
            }
            b.write_fixupp(&payload);
        }
    }

    // MODEND, with the start address when present (frame = target's group via
    // F1/T0? — BCC uses a SEGDEF-framed start, method F0/T4 here).
    if let Some(entry) = &module.entry {
        // flags 0xc0 = main module + start address present; End Data 0x04 =
        // frame method F0 (SEGDEF), P=1, target method T4 (SEGDEF index).
        let mut payload = vec![0xc0u8, 0x04u8];
        payload.push(entry.base_segment); // frame datum (F0 SEGDEF)
        payload.push(entry.base_segment); // target datum (T4 SEGDEF)
        payload.extend_from_slice(&entry.offset.to_le_bytes());
        b.write_record(obj::MODEND_16, &payload);
    } else {
        b.write_modend16_no_entry();
    }

    b.into_bytes()
}

#[cfg(test)]
mod emit_tests {
    use super::*;

    /// Compare the fields `parse` records — everything but the internal
    /// `lnames` index table, which `emit` rebuilds.
    fn assert_semantic_eq(a: &Module, b: &Module) {
        assert_eq!(a.name, b.name, "module name");
        assert_eq!(a.segdefs, b.segdefs, "segdefs");
        assert_eq!(a.grpdefs, b.grpdefs, "grpdefs");
        assert_eq!(a.pubdefs, b.pubdefs, "pubdefs");
        assert_eq!(a.extdefs, b.extdefs, "extdefs");
        assert_eq!(a.entry, b.entry, "entry");
    }

    fn placeholder_seg() -> SegDef {
        SegDef {
            name: String::new(),
            class: String::new(),
            align: 0,
            combine: 0,
            length: 0,
            data: Vec::new(),
            has_data: false,
            fixups: Vec::new(),
        }
    }

    /// A hand-built module — two segments in a group, an extern, publics, an
    /// entry, and fixups in both the near-offset and far-pointer forms —
    /// survives `emit` → `parse` unchanged.
    #[test]
    fn synthetic_roundtrip() {
        let module = Module {
            name: "SYNTH".into(),
            lnames: vec![String::new()],
            segdefs: vec![
                placeholder_seg(),
                SegDef {
                    name: "_TEXT".into(),
                    class: "CODE".into(),
                    align: 1, // byte
                    combine: 2, // public
                    length: 8,
                    data: vec![0xb8, 0, 0, 0x9a, 0, 0, 0, 0],
                    has_data: true,
                    fixups: vec![
                        // mov ax, OFFSET _gv  (near, segment-relative, F1 group, T6 extern)
                        Fixup {
                            seg: 1,
                            data_offset: 1,
                            seg_relative: true,
                            location: 1,
                            frame_method: 1,
                            frame_datum: Some(1),
                            target_method: 6,
                            target_datum: Some(1),
                        },
                        // call far _helper (far pointer, F0 segment, T6 extern)
                        Fixup {
                            seg: 1,
                            data_offset: 4,
                            seg_relative: true,
                            location: 3,
                            frame_method: 0,
                            frame_datum: Some(1),
                            target_method: 6,
                            target_datum: Some(1),
                        },
                    ],
                },
                SegDef {
                    name: "_DATA".into(),
                    class: "DATA".into(),
                    align: 2, // word
                    combine: 2,
                    length: 4,
                    data: vec![1, 0, 2, 0],
                    has_data: true,
                    fixups: Vec::new(),
                },
                SegDef {
                    name: "_BSS".into(),
                    class: "BSS".into(),
                    align: 2,
                    combine: 2,
                    length: 16,
                    data: vec![0; 16],
                    has_data: false,
                    fixups: Vec::new(),
                },
            ],
            grpdefs: vec![GrpDef { name: "DGROUP".into(), segments: vec![2, 3] }],
            pubdefs: vec![PubDef {
                name: "_main".into(),
                base_segment: 1,
                offset: 0,
                absolute_frame: 0,
            }],
            extdefs: vec![String::new(), "_helper".into()],
            entry: Some(Entry { base_segment: 1, offset: 0 }),
        };
        let reparsed = parse(&emit(&module)).expect("emitted object parses");
        assert_semantic_eq(&module, &reparsed);
    }

    /// A real BCC-compiled object (`int main(){return 0;}`) survives the round
    /// trip — `emit` handles the record shapes BCC actually produces.
    #[test]
    fn real_object_roundtrip() {
        let bytes = include_bytes!("../tests/data/MAIN.OBJ");
        let original = parse(bytes).expect("real object parses");
        let reparsed = parse(&emit(&original)).expect("re-emitted object parses");
        assert_semantic_eq(&original, &reparsed);
    }
}
