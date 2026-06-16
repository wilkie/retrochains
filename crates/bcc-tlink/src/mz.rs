//! Serialize a linked [`Image`] into a DOS MZ ("old exe") executable,
//! byte-matching Turbo Link 4.0's output.
//!
//! Header layout (TLINK 4.0, small self-contained image):
//! - bytes 0x00..0x1c: the standard 28-byte MZ header.
//! - bytes 0x1c..0x22: a fixed 6-byte TLINK signature (constant across links;
//!   see [`TLINK_SIGNATURE`]).
//! - byte 0x3e onward: the relocation table (`e_lfarlc` points here).
//! - the whole header is padded up to a 512-byte page boundary (minimum one
//!   page); the load image follows. Far models with many runtime relocations
//!   can push the table past the first page, taking a second (e.g. a huge-model
//!   program with 114 relocations needs `0x206` bytes → a `0x400` header).

use crate::link::Image;

/// Fixed bytes TLINK 4.0 writes at header offset 0x1c, observed identical
/// across distinct links (fixtures 4258 and 4259) regardless of image size —
/// a linker version/identity signature, reproduced verbatim. (Its exact
/// meaning is still unexplained; see specs/linkers/DIFFERENCES.md.)
const TLINK_SIGNATURE: [u8; 6] = [0x01, 0x00, 0xfb, 0x30, 0x6a, 0x72];

/// Offset of the relocation table within the header (`e_lfarlc`). TLINK
/// leaves a gap after the signature and starts relocations at 0x3e.
const RELOC_TABLE_OFFSET: usize = 0x3e;

/// TLINK pads the header up to this size (0x20 paragraphs) for small images.
const HEADER_SIZE: usize = 0x200;

fn put_u16(buf: &mut [u8], at: usize, v: u16) {
    buf[at] = (v & 0xFF) as u8;
    buf[at + 1] = (v >> 8) as u8;
}

/// Serialize `image` to MZ executable bytes.
#[must_use]
pub fn write(image: &Image) -> Vec<u8> {
    // The relocation table sits at 0x3e; the header is padded up to a whole
    // 512-byte page after it (minimum one page). A far-model image with enough
    // relocations to overrun the first page takes a second.
    let reloc_bytes = image.relocations.len() * 4;
    let header_size = (RELOC_TABLE_OFFSET + reloc_bytes).div_ceil(HEADER_SIZE) * HEADER_SIZE;

    let file_size = header_size + image.file_image.len();
    let mut out = vec![0u8; file_size];

    // e_magic
    out[0] = b'M';
    out[1] = b'Z';
    // e_cblp — bytes used on the last 512-byte page (0 means a full page).
    put_u16(&mut out, 0x02, (file_size % 512) as u16);
    // e_cp — number of 512-byte pages, rounding up.
    put_u16(&mut out, 0x04, file_size.div_ceil(512) as u16);
    // e_crlc — relocation count.
    put_u16(&mut out, 0x06, image.relocations.len() as u16);
    // e_cparhdr — header size in paragraphs.
    put_u16(&mut out, 0x08, (header_size / 16) as u16);
    // e_minalloc — extra paragraphs needed beyond the loaded file image.
    let extra = image.mem_size.saturating_sub(image.file_image.len());
    put_u16(&mut out, 0x0a, extra.div_ceil(16) as u16);
    // e_maxalloc.
    put_u16(&mut out, 0x0c, 0xffff);
    // e_ss / e_sp.
    put_u16(&mut out, 0x0e, image.stack_ss);
    put_u16(&mut out, 0x10, image.stack_sp);
    // e_csum — TLINK leaves the checksum zero.
    put_u16(&mut out, 0x12, 0x0000);
    // e_ip / e_cs.
    put_u16(&mut out, 0x14, image.entry_ip);
    put_u16(&mut out, 0x16, image.entry_cs);
    // e_lfarlc — relocation table offset.
    put_u16(&mut out, 0x18, RELOC_TABLE_OFFSET as u16);
    // e_ovno — overlay number (0 = main program).
    put_u16(&mut out, 0x1a, 0x0000);

    // TLINK signature.
    out[0x1c..0x1c + TLINK_SIGNATURE.len()].copy_from_slice(&TLINK_SIGNATURE);

    // Relocation table (empty for self-contained images).
    for (i, &(off, seg)) in image.relocations.iter().enumerate() {
        let at = RELOC_TABLE_OFFSET + i * 4;
        put_u16(&mut out, at, off);
        put_u16(&mut out, at + 2, seg);
    }

    // Load image.
    out[header_size..].copy_from_slice(&image.file_image);
    out
}
