//! Parse an OMF library (`.LIB`) archive into its member modules. The framing
//! (a `0xF0` header that sets the page size, page-aligned members, a trailing
//! dictionary) is documented in `specs/formats/LIB_ARCHIVE.md`. The linker only
//! needs the members themselves — it builds its own symbol→member map from each
//! member's PUBDEFs — so the dictionary is skipped.

use obj::{ObjReader, THEADR};

use crate::omf::{self, Module, ParseError};

const LIBHDR: u8 = 0xf0;
const MODEND_16: u8 = 0x8a;
const MODEND_32: u8 = 0x8b;

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("not an OMF library (expected 0xF0 header, got {0:#x})")]
    NotALibrary(u8),
    #[error("truncated library header")]
    TruncatedHeader,
    #[error("library framing: {0}")]
    Framing(#[from] obj::ReadError),
    #[error("library member at offset {offset}: {source}")]
    Member { offset: usize, source: ParseError },
}

/// Parse every member of a `.LIB` archive into a [`Module`], in archive order.
///
/// # Errors
/// Returns [`ArchiveError`] if the header is malformed or a member isn't valid
/// OMF.
pub fn members(bytes: &[u8]) -> Result<Vec<Module>, ArchiveError> {
    if bytes.len() < 7 {
        return Err(ArchiveError::TruncatedHeader);
    }
    if bytes[0] != LIBHDR {
        return Err(ArchiveError::NotALibrary(bytes[0]));
    }
    // page_size = header record length field + 3 (the type + length bytes).
    let rec_len = usize::from(u16::from(bytes[1]) | (u16::from(bytes[2]) << 8));
    let page_size = rec_len + 3;
    let dict_offset = u32::from(bytes[3])
        | (u32::from(bytes[4]) << 8)
        | (u32::from(bytes[5]) << 16)
        | (u32::from(bytes[6]) << 24);
    let dict_offset = (dict_offset as usize).min(bytes.len());

    let mut out = Vec::new();
    let mut off = page_size;
    while off < dict_offset {
        // Skip inter-member zero padding up to the next member's THEADR.
        while off < dict_offset && bytes[off] == 0 {
            off += 1;
        }
        if off >= dict_offset || bytes[off] != THEADR {
            break;
        }
        // Find the member's extent by reading records up to and including its
        // MODEND, then parse that exact slice as a module.
        let mut reader = ObjReader::new(&bytes[off..]);
        let mut end_rel = 0usize;
        while let Some(rec) = reader.next()? {
            if rec.ty == MODEND_16 || rec.ty == MODEND_32 {
                end_rel = reader.pos();
                break;
            }
        }
        if end_rel == 0 {
            break;
        }
        let member = &bytes[off..off + end_rel];
        let module = omf::parse(member).map_err(|source| ArchiveError::Member { offset: off, source })?;
        out.push(module);
        off += end_rel;
        // Members start on page boundaries.
        off = off.div_ceil(page_size) * page_size;
    }
    Ok(out)
}
