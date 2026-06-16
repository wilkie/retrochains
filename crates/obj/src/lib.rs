//! Intel/Microsoft OMF (Object Module Format) record types used by the
//! Borland C++ 2.0 toolchain. Read and write routines for the records
//! produced by BCC/TASM and consumed by TLINK live here.
//!
//! Each OMF record has the same outer framing:
//!
//! ```text
//! byte 0:        record type
//! bytes 1..2:    record length (little-endian, includes checksum)
//! bytes 3..N-1:  payload
//! byte 3+len-1:  checksum (chosen so the sum of every byte in the
//!                          record, modulo 256, equals 0)
//! ```

// Record type codes BCC emits. The 16-bit and 32-bit variants of each
// record use adjacent codes (e.g. 0x98 vs 0x99 for SEGDEF). BCC under
// the small memory model uses the 16-bit forms; 32-bit forms wait for
// fixtures that need them.
pub const THEADR: u8 = 0x80;
pub const COMENT: u8 = 0x88;
pub const MODEND_16: u8 = 0x8a;
pub const EXTDEF: u8 = 0x8c;
pub const PUBDEF_16: u8 = 0x90;
pub const LNAMES: u8 = 0x96;
pub const SEGDEF_16: u8 = 0x98;
pub const GRPDEF: u8 = 0x9a;
pub const FIXUPP_16: u8 = 0x9c;
pub const LEDATA_16: u8 = 0xa0;
pub const LIDATA_16: u8 = 0xa2;
/// `COMDEF` — communal (tentative) definitions. Each entry names an
/// uninitialized symbol the linker allocates if no PUBDEF defines it; the
/// names share the external-name index space with EXTDEF. MSC emits these for
/// file-scope tentative globals (`int g;`).
pub const COMDEF: u8 = 0xb0;
/// `LCOMDEF` — local (module-private) communal definitions; same payload shape.
pub const LCOMDEF: u8 = 0xb8;
/// `LEXTDEF` — local (module-private) external names. Same payload shape as
/// EXTDEF; used by MSC for `static` function references.
pub const LEXTDEF: u8 = 0xb4;
/// `LPUBDEF` (16-bit) — local (module-private) public definitions. Same
/// payload shape as PUBDEF_16; used by MSC for `static` function definitions.
pub const LPUBDEF_16: u8 = 0xb6;

/// Library-archive header record (LIB file's first byte).
pub const LIBHDR: u8 = 0xf0;
/// Library-archive end-of-file marker.
pub const LIBEND: u8 = 0xf1;
/// Extended-dictionary record at the end of a LIB.
pub const EXTDICT: u8 = 0xf2;

/// Accumulating buffer for an OBJ file. Records are appended via the
/// builder methods; the final bytes come out of `into_bytes()`.
#[derive(Debug, Default)]
pub struct ObjBuilder {
    out: Vec<u8>,
}

impl ObjBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a fully-formed record: type byte, little-endian length
    /// of `payload + 1` (the +1 is for the checksum), the payload
    /// bytes, then the checksum byte computed so the whole record
    /// sums to 0 modulo 256.
    pub fn write_record(&mut self, ty: u8, payload: &[u8]) {
        let len = u16::try_from(payload.len() + 1).expect("record length fits in u16");
        self.out.push(ty);
        self.out.extend_from_slice(&len.to_le_bytes());
        self.out.extend_from_slice(payload);
        let sum: u32 = u32::from(ty)
            + u32::from(len & 0xFF)
            + u32::from((len >> 8) & 0xFF)
            + payload.iter().map(|&b| u32::from(b)).sum::<u32>();
        let checksum = (0u32.wrapping_sub(sum) & 0xFF) as u8;
        self.out.push(checksum);
    }

    /// `THEADR <name>` — module header. `name` is written as a
    /// length-prefixed string ("Pascal-style", 1 length byte + bytes).
    pub fn write_theadr(&mut self, name: &str) {
        let mut payload = Vec::with_capacity(1 + name.len());
        payload.push(u8::try_from(name.len()).expect("module name fits in u8"));
        payload.extend_from_slice(name.as_bytes());
        self.write_record(THEADR, &payload);
    }

    /// `COMENT` with a raw payload that already begins with the
    /// `<flags> <class>` bytes. Callers build the comment payload
    /// themselves — there are too many class-specific shapes to
    /// helper each individually right now.
    pub fn write_coment(&mut self, payload: &[u8]) {
        self.write_record(COMENT, payload);
    }

    /// `LNAMES` — list of length-prefixed names. The empty string is
    /// a valid first entry (and BCC always starts the list with one
    /// — see fixture 002).
    pub fn write_lnames(&mut self, names: &[&str]) {
        let mut payload = Vec::new();
        for name in names {
            payload.push(u8::try_from(name.len()).expect("LNAME fits in u8"));
            payload.extend_from_slice(name.as_bytes());
        }
        self.write_record(LNAMES, &payload);
    }

    /// `SEGDEF` (16-bit form). `acbp` is the alignment/combine/big/
    /// proc byte (BCC uses 0x28 for byte-aligned public _TEXT, 0x48
    /// for word-aligned public _DATA/_BSS). The three name indices
    /// are 1-based into the LNAMES list.
    pub fn write_segdef16(
        &mut self,
        acbp: u8,
        length: u16,
        name_idx: u8,
        class_idx: u8,
        overlay_idx: u8,
    ) {
        let mut payload = Vec::with_capacity(7);
        payload.push(acbp);
        payload.extend_from_slice(&length.to_le_bytes());
        payload.push(name_idx);
        payload.push(class_idx);
        payload.push(overlay_idx);
        self.write_record(SEGDEF_16, &payload);
    }

    /// `GRPDEF` — group of segments. Each segment index is preceded
    /// by a 0xFF marker byte (the "segment-index follows" form;
    /// other forms exist for groups by external/type, but BCC
    /// doesn't use them for `DGROUP`).
    pub fn write_grpdef(&mut self, name_idx: u8, segments: &[u8]) {
        let mut payload = Vec::with_capacity(1 + 2 * segments.len());
        payload.push(name_idx);
        for &seg in segments {
            payload.push(0xFF);
            payload.push(seg);
        }
        self.write_record(GRPDEF, &payload);
    }

    /// `PUBDEF` (16-bit). Defines a public symbol at a known offset
    /// in a segment. `base_group_idx` is 0 when the public is
    /// relative to the base segment directly (which is BCC's
    /// pattern for `_main` at `_TEXT` offset 0).
    pub fn write_pubdef16(
        &mut self,
        base_group_idx: u8,
        base_segment_idx: u8,
        name: &str,
        offset: u16,
        type_idx: u8,
    ) {
        let mut payload = Vec::with_capacity(5 + name.len());
        payload.push(base_group_idx);
        payload.push(base_segment_idx);
        payload.push(u8::try_from(name.len()).expect("public name fits in u8"));
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.push(type_idx);
        self.write_record(PUBDEF_16, &payload);
    }

    /// `LEDATA` (16-bit). Literal data block: places `data` bytes
    /// into `segment_idx` starting at `offset`.
    pub fn write_ledata16(&mut self, segment_idx: u8, offset: u16, data: &[u8]) {
        let mut payload = Vec::with_capacity(3 + data.len());
        payload.push(segment_idx);
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(data);
        self.write_record(LEDATA_16, &payload);
    }

    /// `EXTDEF` — list of external symbol references. `type_idx` is
    /// the TYPDEF index for each name; BCC always emits 0 (untyped).
    /// Each entry contributes a 1-based EXTDEF index, used later by
    /// FIXUPP target datums.
    pub fn write_extdef(&mut self, names: &[&str]) {
        let mut payload = Vec::new();
        for name in names {
            payload.push(u8::try_from(name.len()).expect("EXTDEF name fits"));
            payload.extend_from_slice(name.as_bytes());
            payload.push(0); // type idx
        }
        self.write_record(EXTDEF, &payload);
    }

    /// `FIXUPP` (16-bit form) with a caller-built payload. The payload
    /// is a sequence of FIXUP subrecords (Locat + Fix Data + datums)
    /// or THREAD subrecords. See `specs/formats/OMF.md` §FIXUPP for
    /// the bit layout. Callers serialize subrecords themselves —
    /// each fixup recipe has its own shape and the helpers here would
    /// proliferate quickly.
    pub fn write_fixupp(&mut self, payload: &[u8]) {
        self.write_record(FIXUPP_16, payload);
    }

    /// `MODEND` (16-bit) — end of module. We always use the no-
    /// start-address form (`flags = 0`); a `main` symbol becomes
    /// the entry point via the linker's separate PUBDEF lookup.
    pub fn write_modend16_no_entry(&mut self) {
        self.write_record(MODEND_16, &[0u8]);
    }

    /// Bytes accumulated so far. Consumes the builder.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

/// Iterator-style reader over an OMF byte stream. Walks records
/// from a borrowed `&[u8]`, yielding `Record` views without copying
/// payload bytes. Stops at the end of the input or on a framing
/// error.
#[derive(Debug)]
pub struct ObjReader<'a> {
    data: &'a [u8],
    pos: usize,
}

/// One OMF record borrowed from the input.
#[derive(Debug)]
pub struct Record<'a> {
    /// Record type byte (e.g. 0x80 for THEADR, 0xA0 for LEDATA).
    pub ty: u8,
    /// Payload bytes (excluding the type byte, length field, and
    /// checksum).
    pub payload: &'a [u8],
    /// Checksum byte from the record's tail.
    pub checksum: u8,
    /// Byte offset of the record's first byte within the input.
    /// Useful for diagnostics and for skipping to inter-record
    /// padding.
    pub offset: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("at offset {offset}: truncated record (need {need} bytes, have {have})")]
    Truncated { offset: usize, need: usize, have: usize },
    #[error("at offset {offset}: zero-length record (type {ty:#x})")]
    ZeroLength { offset: usize, ty: u8 },
}

impl<'a> ObjReader<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Byte position of the next record to read. After [`Self::next`]
    /// returns `None` because the stream is exhausted, `pos()` equals
    /// `data.len()`.
    #[must_use]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Fast-forward to a specific byte offset. Used to skip
    /// inter-member padding inside a LIB archive.
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len());
    }

    /// Read one record. Returns `Ok(Some(...))` on success, `Ok(None)`
    /// at end of input, or `Err(...)` on a framing error.
    ///
    /// # Errors
    /// Returns [`ReadError::Truncated`] if fewer bytes remain than the
    /// record's framing says to read.
    pub fn next(&mut self) -> Result<Option<Record<'a>>, ReadError> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let start = self.pos;
        if self.data.len() - start < 3 {
            return Err(ReadError::Truncated {
                offset: start,
                need: 3,
                have: self.data.len() - start,
            });
        }
        let ty = self.data[start];
        let len = u16::from(self.data[start + 1]) | (u16::from(self.data[start + 2]) << 8);
        if len == 0 {
            return Err(ReadError::ZeroLength { offset: start, ty });
        }
        let len_usize = usize::from(len);
        let need = 3 + len_usize;
        if self.data.len() - start < need {
            return Err(ReadError::Truncated {
                offset: start,
                need,
                have: self.data.len() - start,
            });
        }
        let payload = &self.data[start + 3..start + 3 + len_usize - 1];
        let checksum = self.data[start + 3 + len_usize - 1];
        self.pos = start + need;
        Ok(Some(Record { ty, payload, checksum, offset: start }))
    }
}

impl<'a> Record<'a> {
    /// True when the record's bytes sum to 0 mod 256 (the OMF
    /// checksum invariant). Borland tools often emit `0x00` as a
    /// "checksum not present" sentinel, which fails this test;
    /// real consumers (TLINK) accept either. Use this for
    /// verification, not for rejection.
    #[must_use]
    pub fn checksum_valid(&self) -> bool {
        let ty_sum = u32::from(self.ty);
        let payload_sum: u32 = self.payload.iter().map(|&b| u32::from(b)).sum();
        // Length field stored in the record (re-derive from payload size).
        let len = u32::try_from(self.payload.len() + 1).expect("payload fits");
        let len_sum = (len & 0xFF) + ((len >> 8) & 0xFF);
        let total = ty_sum + len_sum + payload_sum + u32::from(self.checksum);
        total % 256 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every record's bytes must sum to 0 mod 256.
    fn assert_checksum(record: &[u8]) {
        let sum: u32 = record.iter().map(|&b| u32::from(b)).sum();
        assert_eq!(sum % 256, 0, "record checksum invalid: {record:02x?}");
    }

    #[test]
    fn theadr_framing() {
        let mut b = ObjBuilder::new();
        b.write_theadr("hello.c");
        let bytes = b.into_bytes();
        // Expected: type 0x80, length 9 (= 1 byte len-prefix + 7 chars + 1 checksum)
        assert_eq!(bytes[0], 0x80);
        assert_eq!(bytes[1..3], [9, 0]);
        // Payload: length byte + "hello.c"
        assert_eq!(bytes[3], 7);
        assert_eq!(&bytes[4..11], b"hello.c");
        assert_checksum(&bytes);
        // Matches the first 12 bytes of fixture 002.
        assert_eq!(
            bytes,
            vec![0x80, 0x09, 0x00, 0x07, b'h', b'e', b'l', b'l', b'o', b'.', b'c', 0xcb]
        );
    }

    #[test]
    fn lnames_starts_with_empty() {
        let mut b = ObjBuilder::new();
        b.write_lnames(&["", "_TEXT", "CODE"]);
        let bytes = b.into_bytes();
        assert_eq!(bytes[0], 0x96);
        // payload: 00 / 05 "_TEXT" / 04 "CODE" / checksum
        assert_eq!(bytes[3], 0); // empty name
        assert_eq!(bytes[4], 5);
        assert_eq!(&bytes[5..10], b"_TEXT");
        assert_eq!(bytes[10], 4);
        assert_eq!(&bytes[11..15], b"CODE");
        assert_checksum(&bytes);
    }

    #[test]
    fn modend_no_entry() {
        let mut b = ObjBuilder::new();
        b.write_modend16_no_entry();
        let bytes = b.into_bytes();
        assert_eq!(bytes, vec![0x8a, 0x02, 0x00, 0x00, 0x74]);
        assert_checksum(&bytes);
    }

    /// Round-trip: write some records, read them back, confirm types
    /// and payloads survive.
    #[test]
    fn reader_roundtrip() {
        let mut b = ObjBuilder::new();
        b.write_theadr("hello.c");
        b.write_lnames(&["", "_TEXT", "CODE"]);
        b.write_modend16_no_entry();
        let bytes = b.into_bytes();

        let mut r = ObjReader::new(&bytes);
        let r1 = r.next().unwrap().unwrap();
        assert_eq!(r1.ty, THEADR);
        assert!(r1.checksum_valid());
        let r2 = r.next().unwrap().unwrap();
        assert_eq!(r2.ty, LNAMES);
        assert!(r2.checksum_valid());
        let r3 = r.next().unwrap().unwrap();
        assert_eq!(r3.ty, MODEND_16);
        assert!(r3.checksum_valid());
        assert!(r.next().unwrap().is_none());
    }
}
