//! Build a `.LIB` archive byte-for-byte like TLIB, from object-file members.
//!
//! Layout (observed against `tlib MYLIB +ADD`; see `specs/bcc/tlib/`):
//! - a `0xF0` library-header record padded to the 16-byte page size;
//! - each member: its OMF stream with the THEADR renamed to the module name
//!   (basename, no extension), padded up to a page boundary;
//! - a `0xF1` LIBEND record padding the member area out to the (512-aligned)
//!   dictionary offset;
//! - the dictionary: 512-byte blocks of the 37-bucket hash table.

use std::collections::HashMap;

use obj::{GRPDEF, LNAMES, ObjBuilder, ObjReader, PUBDEF_16, SEGDEF_16, THEADR};

use crate::dict;

/// The fixed seed of the extended dictionary's names list (indices 0–7); each
/// member's own segment/class/group names are appended after these.
const NAME_SEED: &[&str] = &["", "_TEXT", "_DATA", "_BSS", "DGROUP", "CODE", "DATA", "BSS"];

/// The `packed` segment word for a trivial (standalone, default-attribute)
/// segment. Real attribute-bearing segments use a different encoding that isn't
/// implemented yet — see `specs/bcc/tlib/EXTENDED_DICT.md`.
const SIMPLE_SEG_PACKED: u16 = 0x0160;

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
    #[error("extended dictionary (/E) needs a single-block regular dictionary (got {0})")]
    ExtendedMultiBlock(usize),
}

/// One member ready to archive: the module name, its rewritten OMF bytes, the
/// public symbols it exports, its segments (in SEGDEF order), and the extra
/// names (segment/class/group) it contributes to the extended dictionary.
struct Member {
    name: String,
    bytes: Vec<u8>,
    publics: Vec<String>,
    segments: Vec<String>,
    extra_names: Vec<String>,
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

    // Walk the records: collect publics, the LNAMES table, and the SEGDEF /
    // GRPDEF name references (for the extended dictionary's names list).
    let mut publics = Vec::new();
    let mut lnames: Vec<String> = vec![String::new()]; // 1-based
    let mut segments = Vec::new();
    let mut extra_names = Vec::new();
    let add_name = |n: &str, into: &mut Vec<String>| {
        if !n.is_empty() && !into.contains(&n.to_owned()) {
            into.push(n.to_owned());
        }
    };
    let mut r = ObjReader::new(obj);
    while let Some(rec) = r.next().map_err(|e| WriteError::Framing(module.to_owned(), e))? {
        match rec.ty {
            PUBDEF_16 => {
                let mut p = rec.payload;
                if p.len() >= 2 {
                    p = &p[2..]; // base-group, base-segment
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
            LNAMES => {
                let mut p = rec.payload;
                while !p.is_empty() {
                    let len = usize::from(p[0]);
                    if p.len() < 1 + len {
                        break;
                    }
                    lnames.push(String::from_utf8_lossy(&p[1..1 + len]).into_owned());
                    p = &p[1 + len..];
                }
            }
            SEGDEF_16 => {
                // acbp, length(2), name_idx, class_idx, overlay_idx
                if rec.payload.len() >= 6 {
                    let name = lnames.get(usize::from(rec.payload[3])).cloned().unwrap_or_default();
                    let class = lnames.get(usize::from(rec.payload[4])).cloned().unwrap_or_default();
                    if !name.is_empty() {
                        segments.push(name.clone());
                    }
                    add_name(&name, &mut extra_names);
                    add_name(&class, &mut extra_names);
                }
            }
            GRPDEF => {
                if let Some(&idx) = rec.payload.first() {
                    let group = lnames.get(usize::from(idx)).cloned().unwrap_or_default();
                    add_name(&group, &mut extra_names);
                }
            }
            _ => {}
        }
    }

    Ok(Member { name: module.to_owned(), bytes, publics, segments, extra_names })
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
pub fn build_library(objects: &[(String, Vec<u8>)], extended: bool) -> Result<Vec<u8>, WriteError> {
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

    // Dictionary: `nblocks` 512-byte blocks. `offsets` maps each indexed name to
    // its byte offset within its block (the extended dictionary references these).
    let (dict, offsets) = build_dict(&entries, nblocks)?;
    out.extend_from_slice(&dict);

    if extended {
        if nblocks != 1 {
            return Err(WriteError::ExtendedMultiBlock(nblocks));
        }
        let ext = build_extended_dict(&members, &offsets);
        out.extend_from_slice(&ext);
    }

    write_header(&mut out, dict_offset as u32, nblocks as u16);
    Ok(out)
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.push((v & 0xff) as u8);
    out.push((v >> 8) as u8);
}

/// Build the `/E` extended dictionary for libraries of trivial-segment members.
/// See `specs/bcc/tlib/EXTENDED_DICT.md`; the attribute-bearing segment form
/// (real BCC/C members) is not yet implemented.
fn build_extended_dict(members: &[Member], offsets: &HashMap<String, usize>) -> Vec<u8> {
    // Names list: the fixed seed, then each member's unique segment/class/group
    // names not already present.
    let mut names: Vec<String> = NAME_SEED.iter().map(|s| (*s).to_owned()).collect();
    let mut index: HashMap<String, usize> =
        names.iter().cloned().enumerate().map(|(i, n)| (n, i)).collect();
    for m in members {
        for n in &m.extra_names {
            index.entry(n.clone()).or_insert_with(|| {
                names.push(n.clone());
                names.len() - 1
            });
        }
    }

    let name_count = names.len();
    let total_segs: usize = members.iter().map(|m| m.segments.len()).sum();
    let total_pubs: usize = members.iter().map(|m| m.publics.len()).sum();
    let names_bytes: usize = names.iter().map(|n| 1 + n.len()).sum();

    let mut e = Vec::new();
    // Header.
    push_u16(&mut e, 0x2bad);
    push_u16(&mut e, members.len() as u16);
    push_u16(&mut e, total_segs as u16);
    push_u16(&mut e, total_pubs as u16);
    push_u16(&mut e, name_count as u16);
    push_u16(&mut e, (2 * name_count + names_bytes) as u16);
    // Bucket table — always empty.
    for _ in 0..name_count {
        push_u16(&mut e, 0xffff);
    }
    // Names list, length-prefixed, terminated by an empty name.
    for n in &names {
        e.push(n.len() as u8);
        e.extend_from_slice(n.as_bytes());
    }
    e.push(0);
    // Per-member descriptors, after a one-time 3-byte prefix.
    e.extend_from_slice(&[0, 0, 0]);
    for m in members {
        let modoff = offsets.get(&format!("{}!", m.name)).copied().unwrap_or(0);
        push_u16(&mut e, modoff as u16);
        e.push(0);
        e.push(m.segments.len() as u8);
        e.push(m.publics.len() as u8);
        for seg in &m.segments {
            e.push(0);
            e.push(*index.get(seg).unwrap_or(&0) as u8);
            push_u16(&mut e, SIMPLE_SEG_PACKED);
        }
        for sym in &m.publics {
            let off = offsets.get(sym).copied().unwrap_or(0);
            push_u16(&mut e, off as u16);
            e.extend_from_slice(&[0, 1, 0, 0]);
        }
    }
    e
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
fn build_dict(
    entries: &[(String, u16)],
    nblocks: usize,
) -> Result<(Vec<u8>, HashMap<String, usize>), WriteError> {
    let mut sorted: Vec<&(String, u16)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let mut dict = vec![0u8; nblocks * BLOCK];
    let mut free = vec![DICT_HEADER; nblocks]; // per-block free offset
    let mut offsets: HashMap<String, usize> = HashMap::new();
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
        offsets.insert(name.clone(), off);
        free[block] = off + entry_len;
    }

    // Each block's free-space pointer (byte 37) = next even offset / 2.
    for (i, f) in free.iter().enumerate() {
        dict[i * BLOCK + 37] = (align_up(*f, 2) / 2) as u8;
    }
    Ok((dict, offsets))
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
