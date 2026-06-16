//! The OMF library dictionary — the symbol→page hash index TLIB writes at the
//! end of a `.LIB`. Reverse-engineered against TLIB-built archives; see
//! `specs/tlib/DICTIONARY.md`.
//!
//! A dictionary is one or more 512-byte blocks. Each block is a 37-bucket open
//! hash table: bytes 0..37 are the bucket table (`htab[b]` = entry offset / 2,
//! or 0 if empty), byte 37 is the free-space pointer (/2), and length-prefixed
//! entries (`<len><name><page-u16>`) pack upward from byte 38 on even offsets.
//!
//! ## The hash (confirmed)
//!
//! The bucket index is a 16-bit accumulator folded over the name **back to
//! front**, **lowercasing** each byte:
//!
//! ```text
//! H = 0
//! for c in name.reversed():          // last char first
//!     H = ror16(H, 2) ^ (c | 0x20)   // rotate right 2, xor the lowercased byte
//! bucket = H % 37
//! ```
//!
//! Symbols are inserted in **sorted (ASCII) order**, so on a collision the
//! alphabetically-earlier name keeps the primary bucket and the later one is
//! rehashed. Module members are indexed under their name with a `!` suffix
//! (e.g. `ADD` → `ADD!`).

/// Number of buckets per 512-byte dictionary block.
pub const BUCKETS: u16 = 37;

/// The dictionary bucket for `name` (its primary hash slot, before any
/// collision rehash). The hash lowercases internally, so case doesn't matter.
#[must_use]
pub fn bucket(name: &[u8]) -> u16 {
    hash(name) % BUCKETS
}

/// The raw 16-bit dictionary hash of `name` (bucket = `hash % 37`).
#[must_use]
pub fn hash(name: &[u8]) -> u16 {
    let mut h: u16 = 0;
    for &c in name.iter().rev() {
        h = h.rotate_right(2) ^ u16::from(c | 0x20);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Buckets observed in TLIB-built libraries (single-symbol archives, so
    /// these are collision-free primary slots). See specs/tlib/DICTIONARY.md.
    #[test]
    fn confirmed_buckets() {
        let cases: &[(&str, u16)] = &[
            ("A", 23),
            ("B", 24),
            ("C", 25),
            ("D", 26),
            ("H", 30),
            ("P", 1),
            ("AB", 33),
            ("BA", 4),
            ("AC", 26),
            ("AA", 3),
            ("AAA", 35),
            ("AAAA", 8),
            ("ABC", 6),
            ("CBA", 26),
            ("BAAA", 7),
            ("AAAB", 33),
            // From MYLIB.LIB (fixture 4262): the public and the member name.
            ("ADDONE", 16),
            ("ADD!", 19),
        ];
        for &(name, want) in cases {
            assert_eq!(bucket(name.as_bytes()), want, "bucket({name:?})");
        }
    }

    /// The hash is case-insensitive (it lowercases each byte).
    #[test]
    fn case_insensitive() {
        assert_eq!(hash(b"ADDONE"), hash(b"addone"));
        assert_eq!(hash(b"AbCdEf"), hash(b"ABCDEF"));
    }

    /// The `| 0x20` is applied to *every* byte, not just letters — confirmed
    /// against TLIB by probing identifier chars where `| 0x20` differs from a
    /// real tolower(): `@` (0x40→0x60) and `_` (0x5F→0x7F). An alpha-only
    /// lowercase would put `A@` at bucket 2 and `A_` at 23; TLIB puts them at
    /// 10 and 31 — the `| 0x20` values.
    #[test]
    fn or20_is_unconditional() {
        assert_eq!(bucket(b"A@"), 10);
        assert_eq!(bucket(b"A_"), 31);
        assert_eq!(bucket(b"@A"), 2);
        assert_eq!(bucket(b"_A"), 22);
    }

    /// The hash scans every byte (not a bounded first/last-word hash like BCC's
    /// internal symbol table): changing a *middle* character — same first and
    /// last 16-bit word — changes the bucket.
    #[test]
    fn full_scan_middle_matters() {
        assert_ne!(bucket(b"ABCDE"), bucket(b"ABXDE")); // 30 vs 21
        assert_ne!(bucket(b"ABCDEF"), bucket(b"ABZZEF")); // 15 vs 11
    }
}
