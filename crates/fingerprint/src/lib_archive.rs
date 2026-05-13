//! Walk a Microsoft/Borland .LIB archive and analyze each member.
//! Page sizes are typically 16 bytes for the small libraries shipped
//! with BCC 2.0; see `specs/formats/LIB_ARCHIVE.md` for the framing
//! reference.

use std::collections::BTreeMap;

use obj::ObjReader;

use crate::{AnalyzeError, ObjAnalysis};

#[derive(Debug)]
pub struct LibAnalysis {
    pub page_size: usize,
    pub dictionary_offset: u32,
    pub dictionary_blocks: u16,
    pub flags: u8,
    pub members: Vec<LibMember>,
    /// Histogram of tier classifications across all members.
    pub tier_counts: BTreeMap<&'static str, usize>,
    /// Histogram of memory-model classifications across all members.
    pub model_counts: BTreeMap<&'static str, usize>,
}

#[derive(Debug)]
pub struct LibMember {
    pub start_offset: usize,
    pub analysis: ObjAnalysis,
}

impl LibAnalysis {
    /// "353 BCC-stripped, 169 Borland-ASM" style summary for the
    /// CLI's compact mode.
    pub fn summary_line(&self) -> String {
        let mut parts: Vec<String> = self
            .tier_counts
            .iter()
            .map(|(tier, n)| format!("{n} {tier}"))
            .collect();
        if parts.is_empty() {
            parts.push("0 members".to_string());
        }
        parts.join(", ")
    }
}

/// Parse the library header and walk every member.
///
/// # Errors
/// Returns [`AnalyzeError`] on truncated framing or unexpected layout.
pub fn analyze(data: &[u8]) -> Result<LibAnalysis, AnalyzeError> {
    if data.len() < 10 {
        return Err(AnalyzeError::Empty);
    }
    let rec_len = u16::from(data[1]) | (u16::from(data[2]) << 8);
    let page_size = usize::from(rec_len) + 3;
    let dict_offset = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
    let dict_blocks = u16::from(data[7]) | (u16::from(data[8]) << 8);
    let flags = data[9];

    let mut members = Vec::new();
    let mut tier_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut model_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut pos = page_size;
    let stop = (dict_offset as usize).min(data.len());

    while pos < stop {
        // Skip inter-member padding (zero bytes).
        while pos < stop && data[pos] == 0 {
            pos += 1;
        }
        if pos >= stop {
            break;
        }
        if data[pos] != obj::THEADR {
            // First non-zero byte isn't a THEADR — stop rather than
            // misparse. The dictionary itself starts at dict_offset
            // (which we already bound).
            break;
        }
        // Parse this member as a standalone OBJ. We pass the slice
        // starting at `pos`; the OBJ reader stops at MODEND.
        let mut reader = ObjReader::new(&data[pos..]);
        let mut info = ObjAnalysis::default();
        let mut consumed_within_member = 0usize;
        while let Some(rec) = reader.next()? {
            info.absorb(&rec);
            consumed_within_member = reader.pos();
            if rec.ty == obj::MODEND_16 {
                break;
            }
        }
        let after = pos + consumed_within_member;
        let tier = info.tier();
        *tier_counts.entry(tier.slug()).or_insert(0) += 1;
        *model_counts
            .entry(info.memory_model().slug())
            .or_insert(0) += 1;
        members.push(LibMember { start_offset: pos, analysis: info });
        // Round up to the next page boundary.
        let rem = after % page_size;
        pos = if rem == 0 { after } else { after + (page_size - rem) };
    }

    Ok(LibAnalysis {
        page_size,
        dictionary_offset: dict_offset,
        dictionary_blocks: dict_blocks,
        flags,
        members,
        tier_counts,
        model_counts,
    })
}
