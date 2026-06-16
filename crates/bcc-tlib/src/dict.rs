//! The OMF library dictionary — the symbol→page hash index TLIB writes at the
//! end of a `.LIB`. Fully reverse-engineered (probes + disassembly of
//! `TLIB.EXE`); see `specs/bcc/tlib/DICTIONARY.md`.
//!
//! A dictionary is one or more 512-byte blocks. Each block is a 37-bucket open
//! hash table: bytes 0..37 are the bucket table (`htab[b]` = entry offset / 2,
//! or 0 if empty), byte 37 is the free-space pointer (/2), and length-prefixed
//! entries (`<len><name><page-u16>`) pack upward from byte 38 on even offsets.
//!
//! ## The four hash values
//!
//! TLIB hashes a name into four values in one pass, the classic OMF scheme,
//! folding each byte as `acc = rotate(acc, 2) ^ (b | 0x20)` (the `| 0x20`
//! lowercases *every* byte, unconditionally). The two directions cover
//! *different* byte ranges because the name is **length-prefixed** and the
//! forward pointer starts on the length byte:
//!
//! - **bucket index** — `ror` over the chars **reversed** (no length byte),
//!   `% 37`. This is which bucket the symbol primarily lands in.
//! - **bucket delta** — `ror` over `[len] ++ chars[..len-1]` (the length byte
//!   plus every char *except the last*), `% 37`, forced nonzero. On a collision
//!   the bucket advances by this step: `bucket = (bucket + delta) % 37`.
//! - **block index** — `rol` over `[len] ++ chars[..len-1]`, `% nblocks`.
//! - **block delta** — `rol` over the chars reversed, `% nblocks`, forced
//!   nonzero. When a block's buckets are exhausted, the block advances by this.
//!
//! Symbols are inserted in **sorted (ASCII) order**, so on a collision the
//! alphabetically-earlier name keeps the contested bucket. Module members are
//! indexed under their public symbols plus the member name with a trailing `!`.

/// Number of buckets per 512-byte dictionary block.
pub const BUCKETS: u16 = 37;

/// Fold a byte sequence into the 16-bit hash accumulator, rotating *right* by 2
/// and xor-ing each lowercased byte.
fn fold_ror(bytes: impl Iterator<Item = u8>) -> u16 {
    let mut h: u16 = 0;
    for b in bytes {
        h = h.rotate_right(2) ^ u16::from(b | 0x20);
    }
    h
}

/// Fold rotating *left* by 2 (the block-index/delta variant).
fn fold_rol(bytes: impl Iterator<Item = u8>) -> u16 {
    let mut h: u16 = 0;
    for b in bytes {
        h = h.rotate_left(2) ^ u16::from(b | 0x20);
    }
    h
}

/// The length byte followed by every character except the last — the byte range
/// the forward-scanning accumulators see (the forward pointer starts on the
/// Pascal length byte, and the loop runs `len` times so it stops one short of
/// the final char).
fn len_prefixed_but_last(name: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let len = name.len() as u8;
    std::iter::once(len).chain(name.iter().take(name.len().saturating_sub(1)).copied())
}

/// Primary bucket for `name` in a dictionary block (before any collision
/// rehash). Case-insensitive (the fold lowercases each byte).
#[must_use]
pub fn bucket(name: &[u8]) -> u16 {
    fold_ror(name.iter().rev().copied()) % BUCKETS
}

/// Raw 16-bit bucket-index hash (`bucket = hash % 37`). Kept for tests/tools.
#[must_use]
pub fn hash(name: &[u8]) -> u16 {
    fold_ror(name.iter().rev().copied())
}

/// Collision rehash step within a block: `bucket = (bucket + delta) % 37`.
/// Always nonzero (so it eventually visits every bucket).
#[must_use]
pub fn bucket_delta(name: &[u8]) -> u16 {
    let d = fold_ror(len_prefixed_but_last(name)) % BUCKETS;
    if d == 0 { 1 } else { d }
}

/// Which 512-byte block `name` primarily lands in, for a `nblocks`-block
/// dictionary.
#[must_use]
pub fn block(name: &[u8], nblocks: u16) -> u16 {
    if nblocks == 0 {
        return 0;
    }
    fold_rol(len_prefixed_but_last(name)) % nblocks
}

/// Block rehash step: when a block's buckets are exhausted, advance by this.
/// Always nonzero.
#[must_use]
pub fn block_delta(name: &[u8], nblocks: u16) -> u16 {
    if nblocks == 0 {
        return 1;
    }
    let d = fold_rol(name.iter().rev().copied()) % nblocks;
    if d == 0 { 1 } else { d }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bucket indices observed in TLIB-built libraries (collision-free single-
    /// symbol archives). See specs/bcc/tlib/DICTIONARY.md.
    #[test]
    fn confirmed_buckets() {
        let cases: &[(&str, u16)] = &[
            ("A", 23),
            ("P", 1),
            ("AB", 33),
            ("BA", 4),
            ("AC", 26),
            ("CBA", 26),
            ("AAAA", 8),
            ("ADDONE", 16),
            ("ADD!", 19),
        ];
        for &(name, want) in cases {
            assert_eq!(bucket(name.as_bytes()), want, "bucket({name:?})");
        }
    }

    /// Collision rehash deltas, measured by forced-collision probes and
    /// confirmed against the disassembled TLIB hash routine.
    #[test]
    fn confirmed_bucket_deltas() {
        let cases: &[(&str, u16)] = &[
            // single char → delta from the length byte alone (0x01|0x20=0x21=33)
            ("D", 33),
            ("Q", 33),
            // forward over [len] + all-but-last char
            ("AD", 17),
            ("AE", 17), // last char ignored: AD/AE/AF all 17
            ("AF", 17),
            ("BA", 18),
            ("CA", 19),
            ("CBA", 29),
            ("HHH", 5),
            ("MAAA", 16),
            ("ABCDE", 2),
        ];
        for &(name, want) in cases {
            assert_eq!(bucket_delta(name.as_bytes()), want, "bucket_delta({name:?})");
        }
    }

    /// The `| 0x20` is applied to *every* byte (probed with `@`/`_`, where it
    /// differs from a real tolower): an alpha-only lowercase would give 2/23.
    #[test]
    fn or20_is_unconditional() {
        assert_eq!(bucket(b"A@"), 10);
        assert_eq!(bucket(b"A_"), 31);
    }

    /// Full scan, not a bounded first/last-word hash: a middle-char change moves
    /// the bucket.
    #[test]
    fn full_scan_middle_matters() {
        assert_ne!(bucket(b"ABCDE"), bucket(b"ABXDE"));
        assert_ne!(bucket(b"ABCDEF"), bucket(b"ABZZEF"));
    }

    #[test]
    fn deltas_are_nonzero() {
        // Any name whose raw delta hashes to 0 mod 37 is bumped to 1.
        for n in ["A", "ZZ", "FOO", "QQQQ"] {
            assert_ne!(bucket_delta(n.as_bytes()), 0);
            assert_ne!(block_delta(n.as_bytes(), 4), 0);
        }
    }
}
