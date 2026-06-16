//! Combine parsed modules into a single load image and resolve it: merge
//! same-named segments, order them, assign load addresses, resolve public
//! symbols, and apply fixups. Produces an [`Image`] the MZ writer serializes.

use std::collections::HashMap;

use crate::omf::{Fixup, Module};

/// Bytes-per-unit for an OMF alignment field (SEGDEF ACBP bits 7-5).
fn align_bytes(align: u8) -> usize {
    match align {
        1 => 1,    // byte
        2 => 2,    // word
        4 => 4,    // dword
        5 => 256,  // page
        _ => 16,   // para (3) and anything else
    }
}

fn align_up(value: usize, to: usize) -> usize {
    if to <= 1 { value } else { value.div_ceil(to) * to }
}

/// A segment formed by merging every contributing module segment of the
/// same (name, class).
#[derive(Debug)]
struct Combined {
    name: String,
    class: String,
    align: u8,
    is_stack: bool,
    length: usize,
    data: Vec<u8>,
    has_data: bool,
    /// Byte offset of this segment within the load image (set during layout).
    load_offset: usize,
}

/// One row of the `.MAP` segment table.
#[derive(Debug)]
pub struct MapSegment {
    /// Linear load address (the combined segment's `load_offset`).
    pub start: usize,
    pub length: usize,
    pub name: String,
    pub class: String,
}

/// One `.MAP` public-symbol entry (`frame:offset`). `name` is already
/// upper-cased the way TLINK renders it; `absolute` marks an equate, which
/// the listing tags with `Abs`. `seq` is the symbol's symbol-table insertion
/// order (first encounter as EXTDEF or PUBDEF), used to break `Publics by
/// Value` ties the way TLINK does.
#[derive(Debug)]
pub struct MapPublic {
    pub frame: u16,
    pub offset: u16,
    pub name: String,
    pub absolute: bool,
    pub seq: usize,
}

/// Everything the `.MAP` needs beyond the [`Image`]'s entry point.
#[derive(Debug, Default)]
pub struct MapInfo {
    pub segments: Vec<MapSegment>,
    pub publics: Vec<MapPublic>,
}

/// Where one module's segment landed inside a combined segment.
#[derive(Clone, Copy)]
struct Placement {
    combined: usize,
    base: usize,
}

/// The linked program, ready to serialize as an MZ executable.
#[derive(Debug)]
pub struct Image {
    /// Initialized bytes, from image offset 0 to the last initialized byte.
    pub file_image: Vec<u8>,
    /// Total in-memory size in bytes (including uninitialized BSS/stack).
    pub mem_size: usize,
    /// Entry point paragraph:offset (relative to load).
    pub entry_cs: u16,
    pub entry_ip: u16,
    /// Initial stack paragraph:offset (relative to load).
    pub stack_ss: u16,
    pub stack_sp: u16,
    /// Runtime relocations (segment fixups). Empty for self-contained images.
    pub relocations: Vec<(u16, u16)>,
    /// Segment table + publics for the `.MAP` listing.
    pub map: MapInfo,
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("unresolved external symbol {0:?}")]
    UnresolvedExternal(String),
    #[error("no entry point (no module supplied a MODEND start address)")]
    NoEntry,
    #[error("unsupported fixup: {0}")]
    UnsupportedFixup(String),
    #[error("fixup target segment {0} out of range")]
    BadFixupTarget(u8),
}

/// Link the parsed modules (in command-line order) into a load image.
///
/// # Errors
/// Returns [`LinkError`] for an unresolved external, a missing entry point,
/// or a fixup shape the linker doesn't yet handle.
pub fn link(modules: &[Module]) -> Result<Image, LinkError> {
    let mut combined: Vec<Combined> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    // placements[m][seg_idx] = where module m's SEGDEF #seg_idx landed.
    let mut placements: Vec<Vec<Option<Placement>>> = Vec::with_capacity(modules.len());

    // Pass 1 — merge segments in first-appearance order.
    for module in modules {
        let mut per_module: Vec<Option<Placement>> = vec![None; module.segdefs.len()];
        for (seg_idx, seg) in module.segdefs.iter().enumerate().skip(1) {
            let key = (seg.name.clone(), seg.class.clone());
            let ci = *index.entry(key).or_insert_with(|| {
                combined.push(Combined {
                    name: seg.name.clone(),
                    class: seg.class.clone(),
                    align: seg.align,
                    is_stack: false,
                    length: 0,
                    data: Vec::new(),
                    has_data: false,
                    load_offset: 0,
                });
                combined.len() - 1
            });
            let c = &mut combined[ci];
            c.align = c.align.max(seg.align);
            c.is_stack |= seg.combine == 5;
            // Place this contribution at its alignment within the combined seg.
            let base = align_up(c.length, align_bytes(seg.align));
            let end = base + usize::from(seg.length);
            if end > c.data.len() {
                c.data.resize(end, 0);
            }
            if seg.has_data {
                c.data[base..base + seg.data.len()].copy_from_slice(&seg.data);
                c.has_data = true;
            }
            c.length = c.length.max(end);
            per_module[seg_idx] = Some(Placement { combined: ci, base });
        }
        placements.push(per_module);
    }

    // Pass 1b — reorder combined segments to group by class, classes in
    // first-appearance order and segments within a class in first-appearance
    // order. This reproduces TLINK's DOSSEG-directed segment ordering (the C0
    // startup object carries the `DOSSEG` linker COMENT). Placement indices are
    // remapped so passes 3/4 stay consistent.
    let classes: Vec<String> = combined.iter().map(|c| c.class.clone()).collect();
    let mut class_rank: HashMap<String, usize> = HashMap::new();
    for class in &classes {
        let next = class_rank.len();
        class_rank.entry(class.clone()).or_insert(next);
    }
    let mut order: Vec<usize> = (0..combined.len()).collect();
    order.sort_by_key(|&i| (class_rank[&classes[i]], i));
    let mut remap = vec![0usize; combined.len()];
    for (new_pos, &old) in order.iter().enumerate() {
        remap[old] = new_pos;
    }
    let mut taken: Vec<Option<Combined>> = combined.into_iter().map(Some).collect();
    let mut combined: Vec<Combined> =
        order.iter().map(|&i| taken[i].take().expect("each index taken once")).collect();
    for per_module in &mut placements {
        for slot in per_module.iter_mut().flatten() {
            slot.combined = remap[slot.combined];
        }
    }

    // Group membership per combined segment (which group, if any, owns it) —
    // needed both to pack grouped segments and to frame grouped publics.
    let mut group_names: Vec<String> = Vec::new();
    let mut group_of: Vec<Option<usize>> = vec![None; combined.len()];
    for (m, module) in modules.iter().enumerate() {
        for g in &module.grpdefs {
            let gid = group_names.iter().position(|n| n == &g.name).unwrap_or_else(|| {
                group_names.push(g.name.clone());
                group_names.len() - 1
            });
            for &s in &g.segments {
                if let Some(p) = placements[m].get(usize::from(s)).copied().flatten() {
                    group_of[p.combined] = Some(gid);
                }
            }
        }
    }

    // Pass 2 — lay out combined segments into the image. Every segment honors
    // its own SEGDEF alignment (byte/word/para) and packs against the previous
    // one — so per-module CODE segments (large/medium model `<MODULE>_TEXT`)
    // butt together byte-aligned, and DGROUP's interior segments pack tight.
    // The one exception: the FIRST member of a group starts a fresh paragraph,
    // because a group base (DGROUP) must sit on a paragraph boundary to be a
    // valid frame.
    let mut cursor = 0usize;
    let mut seen_group = vec![false; group_names.len()];
    for (ci, c) in combined.iter_mut().enumerate() {
        let first_of_group = match group_of[ci] {
            Some(g) if !seen_group[g] => {
                seen_group[g] = true;
                true
            }
            _ => false,
        };
        let to = if first_of_group { 16 } else { align_bytes(c.align) };
        cursor = align_up(cursor, to);
        c.load_offset = cursor;
        cursor += c.length;
    }
    let mem_size = cursor;

    // The canonical frame paragraph of each combined segment: the base of the
    // group it belongs to (so a reference to a grouped segment is framed
    // against the group, e.g. DGROUP), or the segment's own load paragraph when
    // ungrouped. Used by both fixup framing and the `.MAP` public listing.
    let mut group_base: Vec<Option<u16>> = vec![None; group_names.len()];
    for (ci, &gid) in group_of.iter().enumerate() {
        if let Some(gid) = gid {
            let para = (combined[ci].load_offset >> 4) as u16;
            group_base[gid] = Some(group_base[gid].map_or(para, |b| b.min(para)));
        }
    }
    let combined_frame: Vec<u16> = (0..combined.len())
        .map(|ci| {
            group_of[ci]
                .and_then(|g| group_base[g])
                .unwrap_or((combined[ci].load_offset >> 4) as u16)
        })
        .collect();

    // Pass 3 — resolve public symbols to (combined segment, absolute address).
    // The combined segment is kept so far fixups can recover the target's
    // frame paragraph.
    let mut symbols: HashMap<String, (usize, usize)> = HashMap::new();
    // Absolute publics (base segment 0): a fixed frame:offset, no segment.
    let mut absolutes: HashMap<String, (u16, u16)> = HashMap::new();
    for (m, module) in modules.iter().enumerate() {
        for pubdef in &module.pubdefs {
            if pubdef.base_segment == 0 {
                absolutes.insert(pubdef.name.clone(), (pubdef.absolute_frame, pubdef.offset));
                continue;
            }
            if let Some(p) = placements[m]
                .get(usize::from(pubdef.base_segment))
                .copied()
                .flatten()
            {
                let addr = combined[p.combined].load_offset + p.base + usize::from(pubdef.offset);
                symbols.insert(pubdef.name.clone(), (p.combined, addr));
            }
        }
    }

    // Symbol-table insertion order, for breaking `Publics by Value` ties — TLINK
    // lists same-address symbols in the order it first entered them. A name is
    // entered the first time it's *seen*, as an external reference (EXTDEF) or a
    // definition (PUBDEF), whichever comes first while scanning modules in link
    // order. So a symbol referenced before its defining library member is pulled
    // (e.g. `_free`, called by the runtime) precedes a same-address alias only
    // defined later in that member (`_farfree`).
    let mut seqs: HashMap<String, usize> = HashMap::new();
    let mut seq_counter = 0usize;
    for module in modules {
        for name in module.extdefs.iter().skip(1).chain(module.pubdefs.iter().map(|p| &p.name)) {
            if !seqs.contains_key(name) {
                seqs.insert(name.clone(), seq_counter);
                seq_counter += 1;
            }
        }
    }

    // Pass 4 — apply fixups (patch the merged segment data in place). Far
    // fixups also contribute a runtime relocation for their segment word.
    let mut relocations: Vec<(u16, u16)> = Vec::new();
    for (m, module) in modules.iter().enumerate() {
        for (seg_idx, seg) in module.segdefs.iter().enumerate().skip(1) {
            let Some(place) = placements[m][seg_idx] else { continue };
            for fx in &seg.fixups {
                if let Some(reloc) = apply_fixup(
                    &mut combined,
                    place,
                    fx,
                    &placements[m],
                    module,
                    &symbols,
                    &absolutes,
                    &combined_frame,
                )? {
                    relocations.push(reloc);
                }
            }
        }
    }

    // Build the file image: bytes up to the last initialized segment's end.
    let mut last_init_end = 0usize;
    for c in &combined {
        if c.has_data {
            last_init_end = last_init_end.max(c.load_offset + c.length);
        }
    }
    let mut file_image = vec![0u8; last_init_end];
    for c in &combined {
        if c.has_data {
            let end = c.load_offset + c.data.len();
            file_image[c.load_offset..end].copy_from_slice(&c.data);
        }
    }

    // Entry point — the module that supplied a MODEND start address.
    let (entry_cs, entry_ip) = resolve_entry(modules, &placements, &combined)?;

    // Initial stack — the (single) stack-combine segment, if any.
    let (stack_ss, stack_sp) = combined
        .iter()
        .find(|c| c.is_stack)
        .map(|c| ((c.load_offset >> 4) as u16, c.length as u16))
        .unwrap_or((0, 0));

    // Map listing — the segment table (in load order) and the publics.
    let segments = combined
        .iter()
        .map(|c| MapSegment {
            start: c.load_offset,
            length: c.length,
            name: c.name.clone(),
            class: c.class.clone(),
        })
        .collect();
    // A public defined in a grouped segment (e.g. DGROUP) is reported relative
    // to the group's base paragraph (see `combined_frame`, built after layout).
    let mut publics: Vec<MapPublic> = symbols
        .iter()
        .map(|(name, &(ci, addr))| {
            let frame = combined_frame[ci];
            MapPublic {
                frame,
                offset: (addr - usize::from(frame) * 16) as u16,
                name: name.to_uppercase(),
                absolute: false,
                seq: seqs.get(name).copied().unwrap_or(usize::MAX),
            }
        })
        .chain(absolutes.iter().map(|(name, &(frame, offset))| MapPublic {
            frame,
            offset,
            name: name.to_uppercase(),
            absolute: true,
            seq: seqs.get(name).copied().unwrap_or(usize::MAX),
        }))
        .collect();
    publics.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Image {
        file_image,
        mem_size,
        entry_cs,
        entry_ip,
        stack_ss,
        stack_sp,
        relocations,
        map: MapInfo { segments, publics },
    })
}

fn resolve_entry(
    modules: &[Module],
    placements: &[Vec<Option<Placement>>],
    combined: &[Combined],
) -> Result<(u16, u16), LinkError> {
    for (m, module) in modules.iter().enumerate() {
        if let Some(entry) = &module.entry {
            let p = placements[m]
                .get(usize::from(entry.base_segment))
                .copied()
                .flatten()
                .ok_or(LinkError::NoEntry)?;
            let addr = combined[p.combined].load_offset + p.base + usize::from(entry.offset);
            let frame = combined[p.combined].load_offset >> 4;
            return Ok((frame as u16, (addr - frame * 16) as u16));
        }
    }
    Err(LinkError::NoEntry)
}

/// The resolved target of a fixup: a concrete image address (segment/extern
/// targets) and/or a frame paragraph (group targets carry only a paragraph).
struct Target {
    /// Absolute image address of the target (0 for a pure group selector).
    addr: usize,
    /// Paragraph of the target's own frame (segment paragraph, or group base).
    frame_para: u16,
}

/// Base paragraph of a group: the lowest load paragraph among its segments.
fn group_base_para(
    module: &Module,
    module_placements: &[Option<Placement>],
    combined: &[Combined],
    grp_idx_1based: u8,
) -> u16 {
    let Some(g) = module.grpdefs.get(usize::from(grp_idx_1based).wrapping_sub(1)) else {
        return 0;
    };
    g.segments
        .iter()
        .filter_map(|&s| module_placements.get(usize::from(s)).copied().flatten())
        .map(|p| combined[p.combined].load_offset)
        .min()
        .map_or(0, |lo| (lo >> 4) as u16)
}

/// Patch one fixup into the merged segment data. Returns a runtime relocation
/// `(offset, segment)` when the fixup deposits a load-relative segment word
/// (segment selectors and far pointers); `None` for fully link-resolved near
/// fixups.
fn apply_fixup(
    combined: &mut [Combined],
    place: Placement,
    fx: &Fixup,
    module_placements: &[Option<Placement>],
    module: &Module,
    symbols: &HashMap<String, (usize, usize)>,
    absolutes: &HashMap<String, (u16, u16)>,
    combined_frame: &[u16],
) -> Result<Option<(u16, u16)>, LinkError> {
    // Absolute image address of the bytes being patched.
    let patch_addr = combined[place.combined].load_offset + place.base + usize::from(fx.data_offset);

    // Resolve the target. T4 = SEGDEF, T5 = GRPDEF (group selector, no own
    // address), T6 = EXTDEF. A target in a grouped segment is framed against
    // the group (`combined_frame`), not the segment's own paragraph.
    let target = match fx.target_method {
        4 => {
            let idx = fx
                .target_datum
                .ok_or_else(|| LinkError::UnsupportedFixup("T4 without datum".into()))?;
            let tp = module_placements
                .get(usize::from(idx))
                .copied()
                .flatten()
                .ok_or(LinkError::BadFixupTarget(idx))?;
            Target {
                addr: combined[tp.combined].load_offset + tp.base,
                frame_para: combined_frame[tp.combined],
            }
        }
        5 => {
            let idx = fx
                .target_datum
                .ok_or_else(|| LinkError::UnsupportedFixup("T5 without datum".into()))?;
            let base = group_base_para(module, module_placements, combined, idx);
            Target { addr: usize::from(base) << 4, frame_para: base }
        }
        6 => {
            let idx = fx
                .target_datum
                .ok_or_else(|| LinkError::UnsupportedFixup("T6 without datum".into()))?;
            let name = module
                .extdefs
                .get(usize::from(idx))
                .ok_or(LinkError::BadFixupTarget(idx))?;
            if let Some(&(ci, addr)) = symbols.get(name) {
                Target { addr, frame_para: combined_frame[ci] }
            } else if let Some(&(frame, offset)) = absolutes.get(name) {
                Target { addr: (usize::from(frame) << 4) + usize::from(offset), frame_para: frame }
            } else {
                return Err(LinkError::UnresolvedExternal(name.clone()));
            }
        }
        other => return Err(LinkError::UnsupportedFixup(format!("target method {other}"))),
    };

    // The frame the reference is measured against. F1 = a named group,
    // F4 = the patched location's own segment (framed against its group if it
    // has one), F5 = the target's frame.
    let frame_para = match fx.frame_method {
        1 => group_base_para(module, module_placements, combined, fx.frame_datum.unwrap_or(0)),
        4 => combined_frame[place.combined],
        _ => target.frame_para,
    };
    let frame_base = usize::from(frame_para) << 4;

    let off = place.base + usize::from(fx.data_offset);
    // Relocation framing: the frame is the patched segment's load paragraph,
    // and the reloc offset is measured from that paragraph. When the segment is
    // byte-packed (large/medium-model code segments don't start on a paragraph),
    // its load address carries a sub-paragraph remainder that must be added to
    // the offset so `frame*16 + offset` still lands on the patched word.
    let loc_frame = (combined[place.combined].load_offset >> 4) as u16;
    let loc_sub = (combined[place.combined].load_offset & 15) as u16;
    let seg = &mut combined[place.combined];

    match fx.location {
        // Near 16-bit offset — resolved fully at link time, no relocation.
        1 => {
            let existing = read_u16(seg, off);
            let value = if fx.seg_relative {
                (target.addr - frame_base) as u16
            } else {
                (target.addr as i32 - (patch_addr as i32 + 2)) as u16
            };
            write_u16(seg, off, existing.wrapping_add(value));
            Ok(None)
        }
        // Segment selector — deposit the frame paragraph, relocate it.
        2 => {
            let existing = read_u16(seg, off);
            write_u16(seg, off, existing.wrapping_add(frame_para));
            Ok(Some((off as u16 + loc_sub, loc_frame)))
        }
        // Far pointer — 16-bit offset (in frame) then the frame segment word.
        3 => {
            let existing_off = read_u16(seg, off);
            write_u16(seg, off, existing_off.wrapping_add((target.addr - frame_base) as u16));
            write_u16(seg, off + 2, frame_para);
            Ok(Some((off as u16 + 2 + loc_sub, loc_frame)))
        }
        other => Err(LinkError::UnsupportedFixup(format!("location type {other}"))),
    }
}

fn read_u16(seg: &Combined, at: usize) -> u16 {
    u16::from(seg.data[at]) | (u16::from(seg.data[at + 1]) << 8)
}

fn write_u16(seg: &mut Combined, at: usize, v: u16) {
    seg.data[at] = (v & 0xFF) as u8;
    seg.data[at + 1] = (v >> 8) as u8;
}
