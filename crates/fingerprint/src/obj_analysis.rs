//! Walk one OMF module and extract every signal we use for
//! fingerprinting. Result is consumed by both the standalone-OBJ
//! path and the LIB-member walker.

use obj::Record;

use crate::FingerprintTier;

/// The TASM-2.51-injected translator string BCC's `-c` flow always
/// produces. Inserted as a length-prefixed string inside a COMENT
/// class 0x00 record.
pub const BCC_TRANSLATOR: &[u8] = b"TC86 Borland Turbo C++ 2.0";

/// The standard BCC LNAMES list — empty-string sentinel first, then
/// segment+class names in declaration order, then group names. Any
/// BCC-compiled OBJ produces this exact list.
pub const BCC_LNAMES: &[&str] = &[
    "", "_TEXT", "CODE", "_DATA", "DATA", "_BSS", "BSS", "DGROUP",
];

/// SEGDEF ACBP bytes BCC emits for the three standard segments.
const BCC_ACBP_TEXT: u8 = 0x28; // byte-aligned, public
const BCC_ACBP_DATA: u8 = 0x48; // word-aligned, public
const BCC_ACBP_BSS: u8 = 0x48;

#[derive(Debug, Default)]
pub struct ObjAnalysis {
    /// THEADR name. `None` if no THEADR was seen (malformed OBJ).
    pub name: Option<String>,
    /// Decoded translator-string payload, if a COMENT class 0x00
    /// was present.
    pub translator: Option<Vec<u8>>,
    /// COMENT class 0xEA (BCC model marker) payload, if present.
    pub ea_marker: Option<Vec<u8>>,
    /// COMENT class 0xE8 (BCC trailing module record) payload.
    pub e8_trailer: Option<Vec<u8>>,
    /// COMENT class 0xA1 (TLIB's archive marker) seen with an
    /// empty payload. Strong signal that the OBJ was extracted
    /// from a Borland LIB.
    pub a1_marker_empty: bool,
    /// All LNAMES entries from the FIRST LNAMES record encountered.
    /// BCC always emits one big LNAMES; MASM-style emission splits
    /// into multiple, in which case this captures only the first.
    pub first_lnames: Option<Vec<String>>,
    /// True when the OBJ contains more than one LNAMES record (a
    /// MASM/TASM-direct-style fingerprint vs. BCC's single block).
    pub multi_lnames: bool,
    /// Raw record types seen, in order. Useful for debug output.
    pub records: Vec<u8>,
    /// SEGDEF ACBP bytes encountered (in segment-declaration order).
    pub segdef_acbps: Vec<u8>,
    /// GRPDEF segment-index lists. BCC always emits one with
    /// `[_DATA, _BSS]` segments — anything else is non-standard.
    pub grpdef_segments: Vec<Vec<u8>>,
    /// First N bytes of the first non-empty LEDATA. Used to spot
    /// BCC's canonical prologue (0x55 0x8B 0xEC).
    pub first_code_bytes: Vec<u8>,
}

impl ObjAnalysis {
    /// Absorb one record into the running analysis.
    pub fn absorb(&mut self, rec: &Record<'_>) {
        self.records.push(rec.ty);
        match rec.ty {
            obj::THEADR => {
                if let Some((&n, rest)) = rec.payload.split_first() {
                    let name_bytes = &rest[..usize::from(n).min(rest.len())];
                    self.name = Some(String::from_utf8_lossy(name_bytes).into_owned());
                }
            }
            obj::COMENT => {
                if rec.payload.len() < 2 {
                    return;
                }
                let class = rec.payload[1];
                let data = &rec.payload[2..];
                match class {
                    0x00 => {
                        // Length-prefixed translator string.
                        if let Some((&n, rest)) = data.split_first() {
                            let len = usize::from(n).min(rest.len());
                            self.translator = Some(rest[..len].to_vec());
                        }
                    }
                    0xE8 => self.e8_trailer = Some(data.to_vec()),
                    0xEA => self.ea_marker = Some(data.to_vec()),
                    0xA1 => {
                        if data.is_empty() {
                            self.a1_marker_empty = true;
                        }
                    }
                    _ => {}
                }
            }
            obj::LNAMES => {
                if self.first_lnames.is_some() {
                    self.multi_lnames = true;
                    return;
                }
                let mut names = Vec::new();
                let mut i = 0;
                while i < rec.payload.len() {
                    let n = rec.payload[i] as usize;
                    i += 1;
                    let end = (i + n).min(rec.payload.len());
                    names.push(String::from_utf8_lossy(&rec.payload[i..end]).into_owned());
                    i = end;
                }
                self.first_lnames = Some(names);
            }
            obj::SEGDEF_16 => {
                if let Some(&acbp) = rec.payload.first() {
                    self.segdef_acbps.push(acbp);
                }
            }
            obj::GRPDEF => {
                // payload: <group-name-idx> (FF <seg-idx>)*
                let mut segs = Vec::new();
                let mut i = 1;
                while i + 1 < rec.payload.len() {
                    if rec.payload[i] == 0xFF {
                        segs.push(rec.payload[i + 1]);
                        i += 2;
                    } else {
                        break;
                    }
                }
                self.grpdef_segments.push(segs);
            }
            obj::LEDATA_16 => {
                if self.first_code_bytes.is_empty() && rec.payload.len() > 3 {
                    let data = &rec.payload[3..]; // skip seg_idx + offset
                    let take = data.len().min(16);
                    self.first_code_bytes = data[..take].to_vec();
                }
            }
            _ => {}
        }
    }

    /// Has the BCC-canonical LNAMES list (single record, exact match).
    #[must_use]
    pub fn matches_bcc_lnames(&self) -> bool {
        if self.multi_lnames {
            return false;
        }
        match &self.first_lnames {
            Some(names) => {
                names.len() == BCC_LNAMES.len()
                    && names.iter().zip(BCC_LNAMES.iter()).all(|(a, b)| a == b)
            }
            None => false,
        }
    }

    /// Has BCC's canonical SEGDEF ACBP sequence (0x28, 0x48, 0x48).
    #[must_use]
    pub fn matches_bcc_segdefs(&self) -> bool {
        self.segdef_acbps == [BCC_ACBP_TEXT, BCC_ACBP_DATA, BCC_ACBP_BSS]
    }

    /// Has the standard `DGROUP = {_DATA, _BSS}` (segments 2, 3) group.
    /// BCC always emits exactly this.
    #[must_use]
    pub fn matches_bcc_grpdef(&self) -> bool {
        self.grpdef_segments.len() == 1
            && self.grpdef_segments[0] == [2, 3]
    }

    /// LEDATA begins with the BCC prologue `55 8B EC` (push bp / mov bp,sp).
    #[must_use]
    pub fn has_bcc_prologue(&self) -> bool {
        self.first_code_bytes
            .starts_with(&[0x55, 0x8B, 0xEC])
    }

    /// True when the translator-string COMENT decodes to BCC's exact
    /// identifying string.
    #[must_use]
    pub fn has_bcc_translator(&self) -> bool {
        self.translator
            .as_deref()
            .map_or(false, |t| t == BCC_TRANSLATOR)
    }

    /// Classify this module into a single tier.
    ///
    /// The tier-2 rule (`Bcc2_0Stripped`) keys on LNAMES + GRPDEF
    /// match. We don't require the exact small-model SEGDEF ACBP
    /// bytes (0x28/0x48/0x48) because BCC's non-small memory models
    /// use different alignment — e.g., CWINS members from BC2.zip
    /// have LNAMES + GRPDEF + BCC's prologue but use 0x48 throughout
    /// (word-aligned `_TEXT`), which we still want to classify as
    /// BCC-compiled. SEGDEF ACBPs surface as evidence in `--verbose`
    /// output for downstream model-detection.
    #[must_use]
    pub fn tier(&self) -> FingerprintTier {
        if self.has_bcc_translator() {
            return FingerprintTier::Bcc2_0Direct;
        }
        if self.matches_bcc_lnames() && self.matches_bcc_grpdef() {
            return FingerprintTier::Bcc2_0Stripped;
        }
        if self.a1_marker_empty {
            // Borland-archived but doesn't match BCC's structural
            // shape — most likely hand-written ASM that went through
            // TASM and TLIB.
            return FingerprintTier::BorlandToolchainAsm;
        }
        FingerprintTier::Unknown
    }
}
