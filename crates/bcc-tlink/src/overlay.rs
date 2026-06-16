//! VROOMM overlay support for the linker. When `/o` marks modules overlaid,
//! TLINK force-pulls the overlay manager from `OVERLAY.LIB`, moves the overlaid
//! code into an appended `FBOV` overlay area behind per-module `INT 3F` stubs,
//! and emits the `__SEGTABLE__` segment table into `_EXEINFO_`. See
//! `specs/bcc/tlink/OVERLAYS.md` for the reverse-engineering.

/// The external reference TLINK injects when `/o` is active, to pull the
/// disk-overlay manager (OVRMAN/OVRUSER/OVRHALT/OVRDATA/OVRHP/OVRBUFF) from
/// `OVERLAY.LIB`.
pub const MANAGER_ROOT: &str = "__OvrPrepare";

/// One resident segment as the overlay segment table sees it.
#[derive(Debug, Clone)]
pub struct ResidentSeg {
    /// Linear load address of the segment.
    pub start: usize,
    /// Initialized + reserved byte length.
    pub len: usize,
    /// Segment class (CODE / DATA / OVRINFO / STUBSEG / …).
    pub class: String,
    /// Group the segment belongs to (`DGROUP`, `_OVRGROUP_`), if any.
    pub group: Option<String>,
    /// True for a generated overlaid-module stub (sets the overlay-stub flag).
    pub overlay_stub: bool,
}

/// `flags` field (entry+4): bit0 = code/stub class, bit1 = overlay stub,
/// bit2 = a non-first member of a group (in load order).
fn flags(seg: &ResidentSeg, first_of_group: bool) -> u16 {
    let mut f = 0u16;
    if seg.class == "CODE" || seg.class == "STUBSEG" {
        f |= 1;
    }
    if seg.overlay_stub {
        f |= 2;
    }
    if seg.group.is_some() && !first_of_group {
        f |= 4;
    }
    f
}

/// `size` field (entry+2): the first member of a group carries the group's
/// extent; later members carry a marker (`0xFFFF` for OVRINFO, else
/// `count-1`); everything else is `count + len` (the length measured from the
/// frame paragraph).
fn size(seg: &ResidentSeg, first_of_group: bool, group_extent: usize) -> u16 {
    let count = (seg.start & 0xf) as u16;
    match &seg.group {
        Some(_) if first_of_group => group_extent as u16,
        Some(_) if seg.class == "OVRINFO" => 0xffff,
        Some(_) => count.wrapping_sub(1),
        None => count + seg.len as u16,
    }
}

/// Build the `__SEGTABLE__` bytes for `_EXEINFO_`: one 8-byte entry per resident
/// segment (in load order) — `[para, size, flags, count]` — followed by the
/// lowercase exe filename (NUL-terminated) and the recorded date tail.
#[must_use]
pub fn segment_table(segs: &[ResidentSeg], exe_name: &str, date_tail: &[u8]) -> Vec<u8> {
    use std::collections::HashMap;
    // Per-group extent (max end − min start) and the first member in load order.
    let mut grp_min: HashMap<&str, usize> = HashMap::new();
    let mut grp_max: HashMap<&str, usize> = HashMap::new();
    for s in segs {
        if let Some(g) = &s.group {
            grp_min.entry(g).and_modify(|m| *m = (*m).min(s.start)).or_insert(s.start);
            grp_max
                .entry(g)
                .and_modify(|m| *m = (*m).max(s.start + s.len))
                .or_insert(s.start + s.len);
        }
    }

    let mut out = Vec::new();
    let mut seen_group: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for s in segs {
        let first = s.group.as_deref().is_some_and(|g| seen_group.insert(g));
        let para = (s.start >> 4) as u16;
        let count = (s.start & 0xf) as u16;
        let extent = s.group.as_deref().map_or(0, |g| grp_max[g] - grp_min[g]);
        let sz = size(s, first, extent);
        let fl = flags(s, first);
        out.extend_from_slice(&para.to_le_bytes());
        out.extend_from_slice(&sz.to_le_bytes());
        out.extend_from_slice(&fl.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
    }
    out.extend_from_slice(exe_name.to_ascii_lowercase().as_bytes());
    out.push(0);
    out.extend_from_slice(date_tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: usize, len: usize, class: &str, group: Option<&str>, stub: bool) -> ResidentSeg {
        ResidentSeg { start, len, class: class.into(), group: group.map(Into::into), overlay_stub: stub }
    }

    /// `segment_table` reproduces the real `_EXEINFO_` for the reference overlay
    /// program (`MAIN` calling overlaid `square`), exercising every field rule:
    /// para/count, the group-extent and `0xFFFF`/`count-1` size cases, and the
    /// flags bitmask (code / overlay-stub / non-first-group-member).
    #[test]
    fn exeinfo_byte_exact() {
        let segs = [
            seg(0x0, 0x1079, "CODE", None, false),
            seg(0x1079, 0x11, "CODE", None, false),
            seg(0x108a, 0x943, "CODE", None, false),
            seg(0x19d0, 0x0, "FAR_DATA", None, false),
            seg(0x19d0, 0x0, "FAR_BSS", None, false),
            seg(0x19d0, 0x0, "OVRINFO", None, false),
            seg(0x19d0, 0x9c, "OVRINFO", Some("_OVRGROUP_"), false),
            seg(0x1a70, 0x1e, "OVRINFO", Some("_OVRGROUP_"), false),
            seg(0x1a90, 0x0, "OVRINFO", Some("_OVRGROUP_"), false),
            seg(0x1a90, 0x0, "OVRINFO", Some("_OVRGROUP_"), false),
            seg(0x1a90, 0x0, "OVRINFO", Some("_OVRGROUP_"), false),
            seg(0x1a90, 0xd8, "OVRINFO", Some("_OVRGROUP_"), false),
            seg(0x1b70, 0x0, "STUBSEG", None, false),
            seg(0x1b70, 0x25, "STUBSEG", None, true),
            seg(0x1ba0, 0x292, "DATA", Some("DGROUP"), false),
            seg(0x1e32, 0x0, "DATA", Some("DGROUP"), false),
            seg(0x1e32, 0x0, "DATA", Some("DGROUP"), false),
            seg(0x1e32, 0x0, "CONST", Some("DGROUP"), false),
            seg(0x1e32, 0xc, "INITDATA", Some("DGROUP"), false),
            seg(0x1e3e, 0x0, "INITDATA", Some("DGROUP"), false),
            seg(0x1e3e, 0x6, "EXITDATA", Some("DGROUP"), false),
            seg(0x1e44, 0x0, "EXITDATA", Some("DGROUP"), false),
            seg(0x1e44, 0x80, "BSS", Some("DGROUP"), false),
            seg(0x1ec4, 0x0, "BSSEND", Some("DGROUP"), false),
            seg(0x1ed0, 0x80, "STACK", None, false),
        ];
        let want: &[u8] = &[
            0x00, 0x00, 0x79, 0x10, 0x01, 0x00, 0x00, 0x00, 0x07, 0x01, 0x1a, 0x00, 0x01, 0x00, 0x09, 0x00,
            0x08, 0x01, 0x4d, 0x09, 0x01, 0x00, 0x0a, 0x00, 0x9d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x9d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x9d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x9d, 0x01, 0x98, 0x01, 0x00, 0x00, 0x00, 0x00, 0xa7, 0x01, 0xff, 0xff, 0x04, 0x00, 0x00, 0x00,
            0xa9, 0x01, 0xff, 0xff, 0x04, 0x00, 0x00, 0x00, 0xa9, 0x01, 0xff, 0xff, 0x04, 0x00, 0x00, 0x00,
            0xa9, 0x01, 0xff, 0xff, 0x04, 0x00, 0x00, 0x00, 0xa9, 0x01, 0xff, 0xff, 0x04, 0x00, 0x00, 0x00,
            0xb7, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xb7, 0x01, 0x25, 0x00, 0x03, 0x00, 0x00, 0x00,
            0xba, 0x01, 0x24, 0x03, 0x00, 0x00, 0x00, 0x00, 0xe3, 0x01, 0x01, 0x00, 0x04, 0x00, 0x02, 0x00,
            0xe3, 0x01, 0x01, 0x00, 0x04, 0x00, 0x02, 0x00, 0xe3, 0x01, 0x01, 0x00, 0x04, 0x00, 0x02, 0x00,
            0xe3, 0x01, 0x01, 0x00, 0x04, 0x00, 0x02, 0x00, 0xe3, 0x01, 0x0d, 0x00, 0x04, 0x00, 0x0e, 0x00,
            0xe3, 0x01, 0x0d, 0x00, 0x04, 0x00, 0x0e, 0x00, 0xe4, 0x01, 0x03, 0x00, 0x04, 0x00, 0x04, 0x00,
            0xe4, 0x01, 0x03, 0x00, 0x04, 0x00, 0x04, 0x00, 0xec, 0x01, 0x03, 0x00, 0x04, 0x00, 0x04, 0x00,
            0xed, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x70, 0x72, 0x6f, 0x67, 0x2e, 0x65, 0x78, 0x65,
            0x00, 0x00, 0x00, 0x70, 0x17, 0x04, 0xc7, 0x07,
        ];
        let date_tail = [0u8, 0, 0x70, 0x17, 0x04, 0xc7, 0x07];
        assert_eq!(segment_table(&segs, "PROG.EXE", &date_tail), want);
    }
}
