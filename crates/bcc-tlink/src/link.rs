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
    align: u8,
    is_stack: bool,
    length: usize,
    data: Vec<u8>,
    has_data: bool,
    /// Byte offset of this segment within the load image (set during layout).
    load_offset: usize,
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

    // Pass 2 — lay out combined segments into the image. Each combined
    // segment starts on a paragraph boundary so it owns a clean frame
    // (CS/SS/group bases are exact paragraph numbers). Within-segment
    // alignment of contributions was already applied in pass 1.
    let mut cursor = 0usize;
    for c in &mut combined {
        cursor = align_up(cursor, 16);
        c.load_offset = cursor;
        cursor += c.length;
    }
    let mem_size = cursor;

    // Pass 3 — resolve public symbols to (combined segment, absolute address).
    // The combined segment is kept so far fixups can recover the target's
    // frame paragraph.
    let mut symbols: HashMap<String, (usize, usize)> = HashMap::new();
    for (m, module) in modules.iter().enumerate() {
        for pubdef in &module.pubdefs {
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

    // Pass 4 — apply fixups (patch the merged segment data in place). Far
    // fixups also contribute a runtime relocation for their segment word.
    let mut relocations: Vec<(u16, u16)> = Vec::new();
    for (m, module) in modules.iter().enumerate() {
        for (seg_idx, seg) in module.segdefs.iter().enumerate().skip(1) {
            let Some(place) = placements[m][seg_idx] else { continue };
            for fx in &seg.fixups {
                if let Some(reloc) =
                    apply_fixup(&mut combined, place, fx, &placements[m], module, &symbols)?
                {
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

    Ok(Image {
        file_image,
        mem_size,
        entry_cs,
        entry_ip,
        stack_ss,
        stack_sp,
        relocations,
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

/// Patch one fixup into the merged segment data. Returns a runtime relocation
/// `(offset, segment)` when the fixup deposits a load-relative segment word
/// (far pointers); `None` for fully link-resolved near fixups.
fn apply_fixup(
    combined: &mut [Combined],
    place: Placement,
    fx: &Fixup,
    module_placements: &[Option<Placement>],
    module: &Module,
    symbols: &HashMap<String, (usize, usize)>,
) -> Result<Option<(u16, u16)>, LinkError> {
    // Absolute image address of the bytes being patched.
    let patch_addr = combined[place.combined].load_offset + place.base + usize::from(fx.data_offset);

    // Resolve the target to its combined segment and absolute image address
    // (T4 = SEGDEF, T6 = EXTDEF). The segment recovers the target's frame.
    let (target_ci, target_addr) = match fx.target_method {
        4 => {
            let idx = fx
                .target_datum
                .ok_or_else(|| LinkError::UnsupportedFixup("T4 without datum".into()))?;
            let tp = module_placements
                .get(usize::from(idx))
                .copied()
                .flatten()
                .ok_or(LinkError::BadFixupTarget(idx))?;
            (tp.combined, combined[tp.combined].load_offset + tp.base)
        }
        6 => {
            let idx = fx
                .target_datum
                .ok_or_else(|| LinkError::UnsupportedFixup("T6 without datum".into()))?;
            let name = module
                .extdefs
                .get(usize::from(idx))
                .ok_or(LinkError::BadFixupTarget(idx))?;
            *symbols.get(name).ok_or_else(|| LinkError::UnresolvedExternal(name.clone()))?
        }
        other => return Err(LinkError::UnsupportedFixup(format!("target method {other}"))),
    };

    let off = place.base + usize::from(fx.data_offset);
    match fx.location {
        // Near 16-bit offset.
        1 => {
            // The frame the offset is measured against. F1 = group frame,
            // F4 = location's frame; F5 (target frame) and the rest use the
            // target's paragraph. For self-relative refs the choice cancels.
            let frame_base = match fx.frame_method {
                1 => {
                    let datum = fx.frame_datum.unwrap_or(0);
                    let grp = module.grpdefs.get(usize::from(datum).wrapping_sub(1));
                    grp.and_then(|g| g.segments.first())
                        .and_then(|&s| module_placements.get(usize::from(s)).copied().flatten())
                        .map_or(0, |p| (combined[p.combined].load_offset >> 4) * 16)
                }
                4 => (combined[place.combined].load_offset >> 4) * 16,
                _ => (target_addr >> 4) * 16,
            };
            let seg = &mut combined[place.combined];
            let existing = u16::from(seg.data[off]) | (u16::from(seg.data[off + 1]) << 8);
            let value = if fx.seg_relative {
                (target_addr - frame_base) as u16
            } else {
                (target_addr as i32 - (patch_addr as i32 + 2)) as u16
            };
            write_u16(seg, off, existing.wrapping_add(value));
            Ok(None)
        }
        // Far pointer: 16-bit offset then 16-bit (load-relative) segment.
        3 => {
            // Target frame = the target segment's paragraph; offset is the
            // target's distance into that segment.
            let target_frame = (combined[target_ci].load_offset >> 4) as u16;
            let target_off = (target_addr - combined[target_ci].load_offset) as u16;
            let seg = &mut combined[place.combined];
            let existing_off = u16::from(seg.data[off]) | (u16::from(seg.data[off + 1]) << 8);
            write_u16(seg, off, existing_off.wrapping_add(target_off));
            write_u16(seg, off + 2, target_frame);

            // Relocate the segment word: its location relative to its own
            // (paragraph-aligned) frame. The patched segment owns frame
            // `load_offset >> 4`, so the in-frame offset is just place+off+2.
            let reloc_frame = (combined[place.combined].load_offset >> 4) as u16;
            let reloc_off = (place.base + usize::from(fx.data_offset) + 2) as u16;
            Ok(Some((reloc_off, reloc_frame)))
        }
        other => Err(LinkError::UnsupportedFixup(format!("location type {other}"))),
    }
}

fn write_u16(seg: &mut Combined, at: usize, v: u16) {
    seg.data[at] = (v & 0xFF) as u8;
    seg.data[at + 1] = (v >> 8) as u8;
}
