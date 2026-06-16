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

/// A communal (tentative) definition from a COMDEF record. Communal names
/// share the external-name index space, so `ext_index` is the name's position
/// in [`Module::extdefs`]; the linker allocates `count * element_size` bytes if
/// no PUBDEF defines the symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComDef {
    /// 1-based index of this symbol's name in [`Module::extdefs`].
    pub ext_index: u8,
    /// `true` = FAR (`0x61`): `count` elements of `element_size` bytes each.
    /// `false` = NEAR (`0x62`): `count` total bytes (`element_size` is 1).
    pub far: bool,
    pub count: u32,
    pub element_size: u32,
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
    /// 1-based EXTDEF names (index 0 unused). Communal (COMDEF) names live here
    /// too — they share the external-name index space.
    pub extdefs: Vec<String>,
    /// Communal (tentative) definitions, each pointing at its name in `extdefs`.
    pub comdefs: Vec<ComDef>,
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
    #[error("FIXUPP references {kind} thread {num}, which was never defined")]
    UndefinedThread { kind: &'static str, num: u8 },
    #[error("unsupported OMF record {0:#x} in linker input")]
    Unsupported(u8),
}

/// FIXUPP thread state: frame and target method+datum pairs a THREAD subrecord
/// pre-registers, referenced by number (0-3) by later FIXUP subrecords. Threads
/// persist across the module's FIXUPP records until redefined.
#[derive(Default)]
struct Threads {
    frame: [Option<(u8, Option<u8>)>; 4],
    target: [Option<(u8, Option<u8>)>; 4],
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
    let mut threads = Threads::default();

    let mut reader = ObjReader::new(bytes);
    while let Some(rec) = reader.next()? {
        match rec.ty {
            obj::THEADR => {
                let mut p = rec.payload;
                module.name = take_pstr(&mut p)?;
            }
            // COMENTs don't affect linking for the BCC pool. One exception we
            // don't yet honor: a class-0x9F default-library directive, which
            // TLINK acts on (e.g. MSC's `SLIBCE`). See
            // specs/bcc/tlink/LIBRARY_RESOLUTION.md.
            obj::COMENT => {}
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
            obj::COMDEF | obj::LCOMDEF => parse_comdef(&mut module, rec.payload)?,
            obj::LEDATA_16 => {
                last_ledata = Some(parse_ledata(&mut module, rec.payload)?);
            }
            obj::LIDATA_16 => {
                last_ledata = Some(parse_lidata(&mut module, rec.payload)?);
            }
            obj::FIXUPP_16 => parse_fixupp(&mut module, rec.payload, last_ledata, &mut threads)?,
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

/// Read a COMDEF variable-length number: a leading byte that is either the
/// value itself (`0x00..=0x80`) or a tag (`0x81`/`0x84`/`0x88`) introducing a
/// 2/3/4-byte little-endian value.
fn take_comdef_num(p: &mut &[u8]) -> Result<u32, ParseError> {
    let lead = take_u8(p)?;
    match lead {
        0x00..=0x80 => Ok(u32::from(lead)),
        0x81 => Ok(u32::from(take_u16(p)?)),
        0x84 => {
            let lo = take_u16(p)?;
            let hi = take_u8(p)?;
            Ok(u32::from(lo) | (u32::from(hi) << 16))
        }
        0x88 => {
            let lo = take_u16(p)?;
            let hi = take_u16(p)?;
            Ok(u32::from(lo) | (u32::from(hi) << 16))
        }
        _ => Err(ParseError::Unsupported(obj::COMDEF)),
    }
}

fn parse_comdef(module: &mut Module, payload: &[u8]) -> Result<(), ParseError> {
    let mut p = payload;
    while !p.is_empty() {
        let name = take_pstr(&mut p)?;
        let _type_idx = take_u8(&mut p)?;
        let data_type = take_u8(&mut p)?;
        let (far, count, element_size) = match data_type {
            0x62 => (false, take_comdef_num(&mut p)?, 1), // NEAR: total length
            0x61 => {
                let count = take_comdef_num(&mut p)?; // number of elements
                let element_size = take_comdef_num(&mut p)?; // bytes per element
                (true, count, element_size)
            }
            _ => return Err(ParseError::Unsupported(obj::COMDEF)),
        };
        // The communal name occupies the next external-name index.
        module.extdefs.push(name);
        let ext_index = u8::try_from(module.extdefs.len() - 1).unwrap_or(0);
        module.comdefs.push(ComDef { ext_index, far, count, element_size });
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
    threads: &mut Threads,
) -> Result<(), ParseError> {
    let mut p = payload;
    while !p.is_empty() {
        let head = take_u8(&mut p)?;
        // A THREAD subrecord (bit 7 = 0) pre-registers a frame or target
        // method+datum under a thread number; it carries no fixup itself. MSC
        // defines all its threads in an early, LEDATA-less FIXUPP record.
        if head & 0x80 == 0 {
            let is_frame = (head & 0x40) != 0;
            let method = (head >> 2) & 0x07;
            let num = usize::from(head & 0x03);
            // Methods that name a segment/group/extern carry an index datum.
            let needs_datum = if is_frame { method <= 2 } else { (method & 0x03) <= 2 };
            let datum = if needs_datum { Some(take_u8(&mut p)?) } else { None };
            if is_frame {
                threads.frame[num] = Some((method, datum));
            } else {
                threads.target[num] = Some((method & 0x03, datum));
            }
            continue;
        }

        // A FIXUP subrecord patches the most recent LEDATA's segment.
        let (seg_idx, ledata_offset) =
            last_ledata.ok_or(ParseError::Truncated { record: "FIXUPP (no LEDATA)" })?;
        let locat_lo = take_u8(&mut p)?;
        let seg_relative = (head & 0x40) != 0;
        let location = (head >> 2) & 0x0F;
        let data_record_offset = (u16::from(head & 0x03) << 8) | u16::from(locat_lo);

        let fix_data = take_u8(&mut p)?;
        let frame_thread = (fix_data & 0x80) != 0;
        let target_thread = (fix_data & 0x08) != 0;
        let p_bit = (fix_data >> 2) & 0x01;

        // Frame from a thread, or inline (datum for methods 0/1/2).
        let (frame_method, frame_datum) = if frame_thread {
            let num = (fix_data >> 4) & 0x03;
            threads.frame[usize::from(num)].ok_or(ParseError::UndefinedThread { kind: "frame", num })?
        } else {
            let frame_method = (fix_data >> 4) & 0x07;
            let frame_datum = if frame_method <= 2 { Some(take_u8(&mut p)?) } else { None };
            (frame_method, frame_datum)
        };

        // Target from a thread (a 2-bit method) or inline; the P bit (from this
        // fixup) extends it to T4-T6 and governs the displacement.
        let (target_low, target_datum) = if target_thread {
            let num = fix_data & 0x03;
            threads.target[usize::from(num)].ok_or(ParseError::UndefinedThread { kind: "target", num })?
        } else {
            (fix_data & 0x03, Some(take_u8(&mut p)?))
        };
        let target_method = (p_bit << 2) | target_low;
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

/// Write a COMDEF variable-length number: a bare byte for `0..=0x80`, else the
/// `0x81`/`0x88` tag plus a 2/4-byte little-endian value.
fn put_comdef_num(payload: &mut Vec<u8>, v: u32) {
    if v <= 0x80 {
        payload.push(u8::try_from(v).expect("guarded ≤ 0x80"));
    } else if v <= 0xFFFF {
        payload.push(0x81);
        payload.extend_from_slice(&u16::try_from(v).expect("guarded ≤ 0xFFFF").to_le_bytes());
    } else {
        payload.push(0x88);
        payload.extend_from_slice(&v.to_le_bytes());
    }
}

/// Emit the external-name records. EXTDEFs and communal COMDEFs share one
/// index space, so walk it in index order, batching each run of regular
/// externs into an EXTDEF record and each run of communals into a COMDEF record
/// (MSC's EXTDEF / COMDEF / EXTDEF split falls out of this naturally).
fn emit_external_names(b: &mut obj::ObjBuilder, module: &Module) {
    let is_communal = |i: usize| module.comdefs.iter().any(|c| usize::from(c.ext_index) == i);
    let mut i = 1usize;
    while i < module.extdefs.len() {
        if is_communal(i) {
            let mut payload = Vec::new();
            while i < module.extdefs.len() {
                let Some(cd) = module.comdefs.iter().find(|c| usize::from(c.ext_index) == i) else {
                    break;
                };
                let name = &module.extdefs[i];
                payload.push(u8::try_from(name.len()).expect("communal name fits in u8"));
                payload.extend_from_slice(name.as_bytes());
                payload.push(0); // type index
                payload.push(if cd.far { 0x61 } else { 0x62 });
                put_comdef_num(&mut payload, cd.count);
                if cd.far {
                    put_comdef_num(&mut payload, cd.element_size);
                }
                i += 1;
            }
            b.write_record(obj::COMDEF, &payload);
        } else {
            let mut names: Vec<&str> = Vec::new();
            while i < module.extdefs.len() && !is_communal(i) {
                names.push(module.extdefs[i].as_str());
                i += 1;
            }
            b.write_extdef(&names);
        }
    }
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

    emit_external_names(&mut b, module);

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

/// A handle to a segment added to a [`ModuleBuilder`] — its 1-based SEGDEF index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegId(u8);

impl SegId {
    /// The 1-based SEGDEF index (e.g. for a fixup frame or target datum).
    #[must_use]
    pub fn index(self) -> u8 {
        self.0
    }
}

/// A handle to a group added to a [`ModuleBuilder`] — its 1-based GRPDEF index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrpId(u8);

impl GrpId {
    /// The 1-based GRPDEF index (e.g. for an F1 fixup frame datum).
    #[must_use]
    pub fn index(self) -> u8 {
        self.0
    }
}

/// Fluent builder for synthetic OMF [`Module`]s. It hides the 1-based index
/// bookkeeping — the index-0 placeholders and the segment / extern / group
/// tables — so a test can describe an object in a few lines, then [`emit`] it
/// (or call [`ModuleBuilder::emit`]) for object bytes. Segment and group adders
/// return handles ([`SegId`] / [`GrpId`]) used to refer back to them; the other
/// setters chain via `&mut Self`.
///
/// # Panics
/// Methods panic if more than 255 segments, externs, or groups are added, or a
/// segment's data exceeds 65535 bytes (the OMF indices and length are single
/// bytes / a u16) — far beyond any synthetic test object.
#[derive(Debug)]
pub struct ModuleBuilder {
    module: Module,
}

/// What an [`extern_ref`](ModuleBuilder::extern_ref) fixup is framed against.
#[derive(Debug, Clone, Copy)]
pub enum Frame {
    /// F4 — the patched location's own segment.
    Location,
    /// F5 — the target's frame.
    Target,
    /// F1 — a named group.
    Group(GrpId),
    /// F0 — a named segment.
    Segment(SegId),
}

impl Frame {
    /// The OMF `(frame_method, frame_datum)` pair this frame encodes.
    fn method_datum(self) -> (u8, Option<u8>) {
        match self {
            Frame::Location => (4, None),
            Frame::Target => (5, None),
            Frame::Group(g) => (1, Some(g.0)),
            Frame::Segment(s) => (0, Some(s.0)),
        }
    }
}

#[allow(clippy::missing_panics_doc)] // documented on the struct
impl ModuleBuilder {
    /// Start a module named `name` (the THEADR).
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            module: Module {
                name: name.into(),
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
            },
        }
    }

    fn push(&mut self, seg: SegDef) -> SegId {
        self.module.segdefs.push(seg);
        SegId(u8::try_from(self.module.segdefs.len() - 1).expect("segment index fits in u8"))
    }

    /// An initialized segment with explicit class / align / combine.
    pub fn segment(&mut self, name: &str, class: &str, align: u8, combine: u8, data: &[u8]) -> SegId {
        self.push(SegDef {
            name: name.into(),
            class: class.into(),
            align,
            combine,
            length: u16::try_from(data.len()).expect("segment fits in u16"),
            data: data.to_vec(),
            has_data: true,
            fixups: Vec::new(),
        })
    }

    /// An uninitialized segment of `len` bytes with explicit class / combine
    /// (word-aligned) — contributes memory size but no LEDATA.
    fn uninit(&mut self, name: &str, class: &str, combine: u8, len: u16) -> SegId {
        self.push(SegDef {
            name: name.into(),
            class: class.into(),
            align: 2,
            combine,
            length: len,
            data: vec![0; usize::from(len)],
            has_data: false,
            fixups: Vec::new(),
        })
    }

    /// A byte-aligned public CODE segment holding `code` (BCC's `_TEXT` shape).
    pub fn code_segment(&mut self, name: &str, code: &[u8]) -> SegId {
        self.segment(name, "CODE", 1, 2, code)
    }

    /// A word-aligned public DATA segment holding `data`.
    pub fn data_segment(&mut self, name: &str, data: &[u8]) -> SegId {
        self.segment(name, "DATA", 2, 2, data)
    }

    /// A word-aligned uninitialized BSS segment of `len` bytes.
    pub fn bss_segment(&mut self, name: &str, len: u16) -> SegId {
        self.uninit(name, "BSS", 2, len)
    }

    /// A stack segment of `len` bytes (combine = stack).
    pub fn stack_segment(&mut self, name: &str, len: u16) -> SegId {
        self.uninit(name, "STACK", 5, len)
    }

    fn extern_idx(&mut self, name: &str) -> u8 {
        if let Some(pos) = self.module.extdefs.iter().position(|n| n == name) {
            u8::try_from(pos).expect("extern index fits in u8")
        } else {
            self.module.extdefs.push(name.into());
            u8::try_from(self.module.extdefs.len() - 1).expect("extern index fits in u8")
        }
    }

    /// Register an external symbol (the fixup helpers register theirs too).
    pub fn extern_(&mut self, name: &str) -> &mut Self {
        self.extern_idx(name);
        self
    }

    /// Register a NEAR communal (tentative) definition of `length` bytes — a
    /// file-scope `int g;` style global. The name joins the external-name index
    /// space; returns its index (usable as a fixup target).
    pub fn comdef(&mut self, name: &str, length: u16) -> u8 {
        let ext_index = self.extern_idx(name);
        self.module.comdefs.push(ComDef {
            ext_index,
            far: false,
            count: u32::from(length),
            element_size: 1,
        });
        ext_index
    }

    /// Define a public symbol at `offset` within `seg`.
    pub fn public(&mut self, name: &str, seg: SegId, offset: u16) -> &mut Self {
        self.module.pubdefs.push(PubDef {
            name: name.into(),
            base_segment: seg.0,
            offset,
            absolute_frame: 0,
        });
        self
    }

    /// Define an absolute public (an equate) at `frame:offset`.
    pub fn absolute(&mut self, name: &str, frame: u16, offset: u16) -> &mut Self {
        self.module.pubdefs.push(PubDef {
            name: name.into(),
            base_segment: 0,
            offset,
            absolute_frame: frame,
        });
        self
    }

    /// Define a group spanning `segs` (e.g. `DGROUP`).
    pub fn group(&mut self, name: &str, segs: &[SegId]) -> GrpId {
        self.module
            .grpdefs
            .push(GrpDef { name: name.into(), segments: segs.iter().map(|s| s.0).collect() });
        GrpId(u8::try_from(self.module.grpdefs.len()).expect("group index fits in u8"))
    }

    /// Set the module entry point to `seg:offset`.
    pub fn entry(&mut self, seg: SegId, offset: u16) -> &mut Self {
        self.module.entry = Some(Entry { base_segment: seg.0, offset });
        self
    }

    /// Add a fully-specified fixup to `seg`.
    pub fn fixup(&mut self, seg: SegId, fixup: Fixup) -> &mut Self {
        self.module.segdefs[usize::from(seg.0)].fixups.push(fixup);
        self
    }

    /// A reference to external `target` (target method T6) at `at` within `seg`,
    /// with the given location type, M (segment-relative) bit, and [`Frame`]. The
    /// external is registered automatically.
    pub fn extern_ref(
        &mut self,
        seg: SegId,
        at: u16,
        location: u8,
        seg_relative: bool,
        frame: Frame,
        target: &str,
    ) -> &mut Self {
        let target_datum = Some(self.extern_idx(target));
        let (frame_method, frame_datum) = frame.method_datum();
        self.fixup(
            seg,
            Fixup {
                seg: seg.0,
                data_offset: at,
                seg_relative,
                location,
                frame_method,
                frame_datum,
                target_method: 6,
                target_datum,
            },
        )
    }

    /// A self-relative near call (`e8`) to external `target`; the displacement
    /// word is at `at` within `seg` (frame [`Frame::Location`]).
    pub fn near_call(&mut self, seg: SegId, at: u16, target: &str) -> &mut Self {
        self.extern_ref(seg, at, 1, false, Frame::Location, target)
    }

    /// A reference to SEGDEF `target` (target method T4) at `at` within `seg`,
    /// with the given location type, M bit, and [`Frame`]. Used for in-module
    /// data references — e.g. `mov ax, [_global]` framed by [`Frame::Group`].
    pub fn segment_ref(
        &mut self,
        seg: SegId,
        at: u16,
        location: u8,
        seg_relative: bool,
        frame: Frame,
        target: SegId,
    ) -> &mut Self {
        let (frame_method, frame_datum) = frame.method_datum();
        self.fixup(
            seg,
            Fixup {
                seg: seg.0,
                data_offset: at,
                seg_relative,
                location,
                frame_method,
                frame_datum,
                target_method: 4,
                target_datum: Some(target.0),
            },
        )
    }

    /// A reference to GRPDEF `target` (target method T5) at `at` within `seg` —
    /// e.g. a group selector `mov ax, DGROUP` (location 2).
    pub fn group_ref(
        &mut self,
        seg: SegId,
        at: u16,
        location: u8,
        seg_relative: bool,
        frame: Frame,
        target: GrpId,
    ) -> &mut Self {
        let (frame_method, frame_datum) = frame.method_datum();
        self.fixup(
            seg,
            Fixup {
                seg: seg.0,
                data_offset: at,
                seg_relative,
                location,
                frame_method,
                frame_datum,
                target_method: 5,
                target_datum: Some(target.0),
            },
        )
    }

    /// Finish, returning the [`Module`].
    #[must_use]
    pub fn build(self) -> Module {
        self.module
    }

    /// Finish and serialize to OMF object bytes (`emit(&self.build())`).
    #[must_use]
    pub fn emit(self) -> Vec<u8> {
        emit(&self.module)
    }
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
        assert_eq!(a.comdefs, b.comdefs, "comdefs");
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
            comdefs: Vec::new(),
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

    /// The `ModuleBuilder` produces an object equivalent to the hand-built one:
    /// grouped data/bss segments, and external references framed by a group
    /// (F1) and by a segment (F0). Confirms the builder + every `Frame` variant
    /// it exercises round-trips through `emit`/`parse`.
    #[test]
    fn builder_roundtrip() {
        let mut b = ModuleBuilder::new("BLD");
        let text = b.code_segment("_TEXT", &[0xb8, 0, 0, 0x9a, 0, 0, 0, 0]);
        let data = b.data_segment("_DATA", &[1, 0, 2, 0]);
        let bss = b.bss_segment("_BSS", 16);
        let dgroup = b.group("DGROUP", &[data, bss]);
        b.extern_ref(text, 1, 1, true, Frame::Group(dgroup), "_gv"); // mov ax, OFFSET _gv
        b.extern_ref(text, 4, 3, true, Frame::Segment(text), "_helper"); // call far _helper
        b.public("_main", text, 0).entry(text, 0);

        let module = b.build();
        let reparsed = parse(&emit(&module)).expect("builder object parses");
        assert_semantic_eq(&module, &reparsed);
    }

    /// A communal (COMDEF) sandwiched between regular externs round-trips with
    /// the external-name index order intact — the layout MSC emits for a
    /// tentative global between runtime helpers — and a fixup targeting the
    /// communal keeps pointing at it.
    #[test]
    fn comdef_roundtrip() {
        let mut b = ModuleBuilder::new("COMM");
        let text = b.code_segment("_TEXT", &[0xa1, 0, 0, 0xc3]); // mov ax,[_g]; ret
        b.extern_("__chkstk"); // ext index 1
        b.comdef("_g", 2); // ext index 2 (communal)
        b.extern_("_printf"); // ext index 3
        b.extern_ref(text, 1, 1, true, Frame::Location, "_g"); // references the communal
        b.public("_main", text, 0).entry(text, 0);

        let module = b.build();
        assert_eq!(module.extdefs, ["", "__chkstk", "_g", "_printf"]);
        assert_eq!(module.comdefs, [ComDef { ext_index: 2, far: false, count: 2, element_size: 1 }]);

        let bytes = emit(&module);
        // The COMDEF entry matches real MSC byte-for-byte: for `int g;` MSC emits
        // `02 5f 67 00 62 02` (name "_g", type 0, NEAR 0x62, length 2).
        assert!(
            bytes.windows(6).any(|w| w == [0x02, b'_', b'g', 0x00, 0x62, 0x02]),
            "COMDEF entry matches MSC's `02 5f 67 00 62 02`",
        );
        assert_semantic_eq(&module, &parse(&bytes).expect("communal object parses"));
    }

    /// A FIXUP that references a pre-registered target THREAD resolves to the
    /// same fixup an inline encoding would, with the P bit extending the
    /// thread's 2-bit method to T6.
    #[test]
    fn fixupp_thread_resolves() {
        let mut b = obj::ObjBuilder::new();
        b.write_theadr("THR");
        b.write_lnames(&["", "_TEXT", "CODE"]);
        b.write_segdef16(0x28, 3, 2, 3, 0); // _TEXT, CODE, byte/public
        b.write_extdef(&["_ext"]); // extern index 1
        b.write_fixupp(&[0x08, 0x01]); // THREAD: target 0 = EXTDEF (method 2), datum 1
        b.write_ledata16(1, 0, &[0xe8, 0, 0]); // call _ext
        // FIXUP: loc 1 (offset), seg-rel; frame F4 inline; target via thread 0, P=1.
        b.write_fixupp(&[0xc4, 0x01, 0x4c]);
        b.write_modend16_no_entry();

        let module = parse(&b.into_bytes()).expect("thread object parses");
        assert_eq!(
            module.segdefs[1].fixups,
            [Fixup {
                seg: 1,
                data_offset: 1,
                seg_relative: true,
                location: 1,
                frame_method: 4,
                frame_datum: None,
                target_method: 6, // (P=1 << 2) | thread method 2
                target_datum: Some(1), // _ext, from the thread
            }],
        );
    }

    /// A real MSC object (`int g; int main(){return g;}`) — which carries the
    /// pre-registered FIXUPP threads and a COMDEF that previously stopped the
    /// parser — parses and survives the round trip.
    #[test]
    fn msc_object_with_threads_roundtrip() {
        let bytes = include_bytes!("../tests/data/COMM_MSC.OBJ");
        let original = parse(bytes).expect("MSC object parses");
        assert_eq!(&original.extdefs[1..], ["__acrtused", "__chkstk", "_g", "_main"]);
        assert_eq!(original.comdefs, [ComDef { ext_index: 3, far: false, count: 2, element_size: 1 }]);
        let reparsed = parse(&emit(&original)).expect("re-emitted MSC object parses");
        assert_semantic_eq(&original, &reparsed);
    }

    /// The COMDEF length field: `≤ 0x80` is a bare byte, larger uses the `0x81`
    /// tag plus a little-endian u16.
    #[test]
    fn comdef_length_encoding() {
        let mut b = ModuleBuilder::new("C");
        b.comdef("_small", 2);
        b.comdef("_big", 0x100);
        let bytes = b.emit();
        assert!(bytes.windows(2).any(|w| w == [0x62, 0x02]), "NEAR small length = 0x62 0x02");
        assert!(
            bytes.windows(4).any(|w| w == [0x62, 0x81, 0x00, 0x01]),
            "NEAR big length = 0x62 0x81 <u16 0x0100>",
        );
        let m = parse(&bytes).expect("parses");
        assert_eq!(m.comdefs.iter().map(|c| c.count).collect::<Vec<_>>(), [2, 0x100]);
    }
}
