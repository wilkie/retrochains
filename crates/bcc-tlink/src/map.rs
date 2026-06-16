//! Format a linked [`Image`] into TLINK's `.MAP` listing, byte-for-byte.
//!
//! The listing (CRLF line endings) is: a segment table (start/stop/length, in
//! load order), the publics sorted by name, the publics sorted by value, and
//! the program entry point. Verified against the standalone-linker fixtures.

use crate::link::{Image, MapPublic};

/// Render the `.MAP` text for `image`.
#[must_use]
pub fn format(image: &Image) -> Vec<u8> {
    let mut s = String::new();
    s.push_str("\r\n");
    s.push_str(" Start  Stop   Length Name               Class\r\n");
    s.push_str("\r\n");
    for seg in &image.map.segments {
        let stop = if seg.length == 0 { seg.start } else { seg.start + seg.length - 1 };
        s.push_str(&format!(
            " {:05X}H {:05X}H {:05X}H {:<19}{}\r\n",
            seg.start, stop, seg.length, seg.name, seg.class
        ));
    }
    s.push_str("\r\n");

    // Publics by name (already name-sorted by the linker).
    s.push_str("  Address         Publics by Name\r\n\r\n");
    for p in &image.map.publics {
        push_public(&mut s, p);
    }
    s.push_str("\r\n");

    // Publics by value (frame:offset).
    s.push_str("  Address         Publics by Value\r\n\r\n");
    // Absolute equates group first (sorted by offset), then relocatable
    // symbols by (frame, offset); ties break by definition order.
    let mut by_value: Vec<&MapPublic> = image.map.publics.iter().collect();
    by_value.sort_by_key(|p| (!p.absolute, p.frame, p.offset, p.seq));
    for p in by_value {
        push_public(&mut s, p);
    }
    s.push_str("\r\n");

    s.push_str(&format!(
        "Program entry point at {:04X}:{:04X}\r\n\r\n",
        image.entry_cs, image.entry_ip
    ));
    s.into_bytes()
}

fn push_public(s: &mut String, p: &MapPublic) {
    // Absolute equates carry an `Abs` tag in the 7-column gap after the
    // address; ordinary symbols leave it blank.
    let gap = if p.absolute { "  Abs  " } else { "       " };
    s.push_str(&format!(" {:04X}:{:04X}{}{}\r\n", p.frame, p.offset, gap, p.name));
}
