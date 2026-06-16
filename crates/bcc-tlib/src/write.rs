//! Build a `.LIB` archive byte-for-byte like TLIB, from object-file members.
//!
//! Layout (observed against `tlib MYLIB +ADD`; see `specs/bcc/tlib/`):
//! - a `0xF0` library-header record padded to the 16-byte page size;
//! - each member: its OMF stream with the THEADR renamed to the module name
//!   (basename, no extension), padded up to a page boundary;
//! - a `0xF1` LIBEND record padding the member area out to the (512-aligned)
//!   dictionary offset;
//! - the dictionary: 512-byte blocks of the 37-bucket hash table.

use obj::{ObjBuilder, ObjReader, PUBDEF_16, THEADR};

use crate::dict;

/// Page size for BC2/TLIB libraries (member alignment + header length+3).
const PAGE: usize = 16;
/// Dictionary blocks are 512 bytes and start on a 512-byte boundary.
const BLOCK: usize = 512;
/// Entry header size in a dictionary block: 37 bucket bytes + 1 free pointer.
const DICT_HEADER: usize = 38;

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("member {0:?} does not start with a THEADR record")]
    NoTheadr(String),
    #[error("OMF framing in member {0:?}: {1}")]
    Framing(String, obj::ReadError),
    #[error("dictionary overflow: {0} entries did not fit the allocated blocks")]
    DictOverflow(usize),
}

/// One member ready to archive: the module name, its rewritten OMF bytes, and
/// the public symbols it exports.
struct Member {
    name: String,
    bytes: Vec<u8>,
    publics: Vec<String>,
}

/// Rewrite an input object's THEADR to `module` and collect its PUBDEF names.
/// The rest of the OMF stream is copied verbatim (TLIB leaves it untouched for
/// TASM objects).
fn prepare(module: &str, obj: &[u8]) -> Result<Member, WriteError> {
    let mut reader = ObjReader::new(obj);
    let first = reader
        .next()
        .map_err(|e| WriteError::Framing(module.to_owned(), e))?
        .filter(|r| r.ty == THEADR)
        .ok_or_else(|| WriteError::NoTheadr(module.to_owned()))?;
    let _ = first;
    let theadr_end = reader.pos();

    let mut bytes = {
        let mut b = ObjBuilder::new();
        b.write_theadr(module);
        b.into_bytes()
    };
    bytes.extend_from_slice(&obj[theadr_end..]);

    // Collect public symbols (PUBDEF only; locals don't enter the dictionary).
    let mut publics = Vec::new();
    let mut r = ObjReader::new(obj);
    while let Some(rec) = r.next().map_err(|e| WriteError::Framing(module.to_owned(), e))? {
        if rec.ty == PUBDEF_16 {
            let mut p = rec.payload;
            // skip base-group, base-segment
            if p.len() >= 2 {
                p = &p[2..];
            }
            while !p.is_empty() {
                let len = usize::from(p[0]);
                if p.len() < 1 + len + 3 {
                    break;
                }
                publics.push(String::from_utf8_lossy(&p[1..1 + len]).into_owned());
                p = &p[1 + len + 3..]; // name + offset(2) + type(1)
            }
        }
    }

    Ok(Member { name: module.to_owned(), bytes, publics })
}

fn align_up(v: usize, to: usize) -> usize {
    v.div_ceil(to) * to
}

/// Build a `.LIB` from `(module_name, object_bytes)` pairs, in command order.
///
/// # Errors
/// Returns [`WriteError`] if a member isn't a valid OMF stream starting with a
/// THEADR, or if the dictionary would need more than one block (multi-block
/// dictionaries are understood but not yet emitted).
pub fn build_library(objects: &[(String, Vec<u8>)]) -> Result<Vec<u8>, WriteError> {
    let members: Vec<Member> = objects
        .iter()
        .map(|(name, bytes)| prepare(name, bytes))
        .collect::<Result<_, _>>()?;

    // Header page (filled in at the end), then members on page boundaries.
    let mut out = vec![0u8; PAGE];
    // Dictionary entries: (name, page-number).
    let mut entries: Vec<(String, u16)> = Vec::new();
    for m in &members {
        let page = (out.len() / PAGE) as u16;
        out.extend_from_slice(&m.bytes);
        out.resize(align_up(out.len(), PAGE), 0);
        // The member name (with a trailing '!') and each public are indexed.
        entries.push((format!("{}!", m.name), page));
        for sym in &m.publics {
            entries.push((sym.clone(), page));
        }
    }

    // LIBEND record pads the member area out to the 512-aligned dictionary.
    let nblocks = dict_block_count(&entries);
    let libend_pos = out.len();
    let dict_offset = align_up(libend_pos, BLOCK);
    let libend_len = dict_offset - libend_pos - 3;
    out.push(0xf1);
    out.push((libend_len & 0xff) as u8);
    out.push((libend_len >> 8) as u8);
    out.resize(dict_offset, 0);

    // Dictionary: `nblocks` 512-byte blocks.
    let dict = build_dict(&entries, nblocks)?;
    out.extend_from_slice(&dict);

    write_header(&mut out, dict_offset as u32, nblocks as u16);
    Ok(out)
}

/// The byte size an entry occupies, as TLIB counts it for block sizing:
/// `(namelen + 4)` rounded down to even (equivalently the even-aligned
/// `<len><name><page>` record).
fn entry_bytes(name: &str) -> usize {
    (name.len() + 4) & !1
}

/// Number of 512-byte dictionary blocks TLIB allocates (from the disassembled
/// sizing routine): `max(1, ceil(count/35), ceil((bytes-128)/346))`, where the
/// bucket bound divides by 35 (not 37 — two buckets of headroom) and the byte
/// bound reserves 128 bytes per block.
fn dict_block_count(entries: &[(String, u16)]) -> usize {
    let count = entries.len();
    let bytes: usize = entries.iter().map(|(n, _)| entry_bytes(n)).sum();
    let bucket_based = (count + 34) / 35;
    let byte_based = (bytes + 217) / 346; // ceil((bytes - 128) / 346)
    bucket_based.max(byte_based).max(1)
}

/// Lay out the dictionary. Entries are inserted in sorted name order; each
/// hashes to a `(block, bucket)` and, on collision, rehashes by `bucket_delta`
/// within the block, advancing to `(block + block_delta) % nblocks` when a
/// block's buckets are exhausted.
fn build_dict(entries: &[(String, u16)], nblocks: usize) -> Result<Vec<u8>, WriteError> {
    let mut sorted: Vec<&(String, u16)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let mut dict = vec![0u8; nblocks * BLOCK];
    let mut free = vec![DICT_HEADER; nblocks]; // per-block free offset
    let nbuckets = usize::from(dict::BUCKETS);

    for (name, page) in sorted {
        let nb = name.as_bytes();
        let bucket0 = usize::from(dict::bucket(nb));
        let bdelta = usize::from(dict::bucket_delta(nb));
        let block0 = usize::from(dict::block(nb, nblocks as u16));
        let blkdelta = usize::from(dict::block_delta(nb, nblocks as u16));

        // Probe for a free bucket.
        let (mut block, mut bucket) = (block0, bucket0);
        let mut guard = 0;
        while dict[block * BLOCK + bucket] != 0 {
            bucket = (bucket + bdelta) % nbuckets;
            if bucket == bucket0 {
                block = (block + blkdelta) % nblocks;
                bucket = bucket0;
                if block == block0 {
                    return Err(WriteError::DictOverflow(entries.len()));
                }
            }
            guard += 1;
            if guard > nblocks * nbuckets {
                return Err(WriteError::DictOverflow(entries.len()));
            }
        }

        // Place the entry at the chosen block's free offset (even-aligned).
        let base = block * BLOCK;
        let off = align_up(free[block], 2);
        let entry_len = 1 + nb.len() + 2;
        if off + entry_len > BLOCK {
            return Err(WriteError::DictOverflow(entries.len()));
        }
        dict[base + bucket] = (off / 2) as u8;
        dict[base + off] = nb.len() as u8;
        dict[base + off + 1..base + off + 1 + nb.len()].copy_from_slice(nb);
        dict[base + off + 1 + nb.len()] = (*page & 0xff) as u8;
        dict[base + off + 2 + nb.len()] = (*page >> 8) as u8;
        free[block] = off + entry_len;
    }

    // Each block's free-space pointer (byte 37) = next even offset / 2.
    for (i, f) in free.iter().enumerate() {
        dict[i * BLOCK + 37] = (align_up(*f, 2) / 2) as u8;
    }
    Ok(dict)
}

/// Write the `0xF0` header record into the reserved first page.
fn write_header(out: &mut [u8], dict_offset: u32, dict_blocks: u16) {
    // Record length is page_size - 3 (the record fills the whole first page).
    let rec_len = (PAGE - 3) as u16;
    out[0] = 0xf0;
    out[1] = (rec_len & 0xff) as u8;
    out[2] = (rec_len >> 8) as u8;
    out[3] = (dict_offset & 0xff) as u8;
    out[4] = ((dict_offset >> 8) & 0xff) as u8;
    out[5] = ((dict_offset >> 16) & 0xff) as u8;
    out[6] = ((dict_offset >> 24) & 0xff) as u8;
    out[7] = (dict_blocks & 0xff) as u8;
    out[8] = (dict_blocks >> 8) as u8;
    out[9] = 0; // flags
    // bytes 10..16 stay zero (header padding within the record).
}
