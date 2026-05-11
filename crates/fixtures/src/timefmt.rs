//! Minimal RFC3339 (UTC) formatting for `SystemTime`. Keeps the manifest
//! human-readable without dragging in chrono.

use std::time::{SystemTime, UNIX_EPOCH};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Format an instant as `YYYY-MM-DDTHH:MM:SSZ`. Returns `None` for instants
/// outside the representable range (essentially never, in practice).
pub fn format(instant: SystemTime) -> Option<String> {
    let secs = instant.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let i64_secs = i64::try_from(secs).ok()?;
    let odt = OffsetDateTime::from_unix_timestamp(i64_secs).ok()?;
    odt.format(&Rfc3339).ok()
}

/// Parse a string produced by [`format`] back into a `SystemTime`. Not
/// currently used by the harness — kept for future verification paths that
/// need to compare a manifest-mtime against an actual filesystem mtime
/// numerically rather than as a string.
#[allow(dead_code)]
pub fn parse(s: &str) -> Option<SystemTime> {
    let odt = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    let secs = odt.unix_timestamp();
    let u_secs = u64::try_from(secs).ok()?;
    Some(UNIX_EPOCH + std::time::Duration::from_secs(u_secs))
}
