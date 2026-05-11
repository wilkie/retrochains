//! DOS-packed date/time. BCC stamps the source file's mtime (converted to
//! DOS packed format) into the `?debug C` record, so we need to produce
//! the same value.
//!
//! Layout (32-bit little-endian when written as 4 bytes):
//! - bits 31..25 (7 bits): year - 1980
//! - bits 24..21 (4 bits): month (1..12)
//! - bits 20..16 (5 bits): day (1..31)
//! - bits 15..11 (5 bits): hour (0..23)
//! - bits 10..5  (6 bits): minute (0..59)
//! - bits 4..0   (5 bits): seconds / 2 (0..29)
//!
//! Times are interpreted as UTC; the oracle pins `TZ=UTC` so this matches.

use std::time::SystemTime;

use time::OffsetDateTime;

/// Convert a `SystemTime` to a DOS-packed 32-bit timestamp.
#[must_use]
pub fn pack(t: SystemTime) -> u32 {
    let Ok(odt) = OffsetDateTime::from_unix_timestamp(unix_secs(t)) else {
        return 0;
    };
    let year = u32::try_from(odt.year().saturating_sub(1980)).unwrap_or(0) & 0x7F;
    let month = u32::from(u8::from(odt.month())) & 0x0F;
    let day = u32::from(odt.day()) & 0x1F;
    let hour = u32::from(odt.hour()) & 0x1F;
    let minute = u32::from(odt.minute()) & 0x3F;
    let second = u32::from(odt.second() / 2) & 0x1F;
    (year << 25) | (month << 21) | (day << 16) | (hour << 11) | (minute << 5) | second
}

fn unix_secs(t: SystemTime) -> i64 {
    use std::time::UNIX_EPOCH;
    if let Ok(d) = t.duration_since(UNIX_EPOCH) {
        i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
    } else if let Ok(d) = UNIX_EPOCH.duration_since(t) {
        -i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    #[allow(clippy::duration_suboptimal_units)]
    fn pins_to_expected_value_for_1991_04_23_noon_utc() {
        // 672408000 = 1991-04-23 12:00:00 UTC.
        let t = UNIX_EPOCH + Duration::from_secs(672_408_000);
        // From specs/bcc/ASM_OUTPUT.md: the captured value is little-endian
        // `00 60 97 16` → 0x16976000.
        assert_eq!(pack(t), 0x1697_6000);
    }
}
