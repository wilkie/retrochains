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
    /// Full data payload of the first non-empty LEDATA encountered
    /// (skipping LEDATA's 3-byte `<seg> <off>` header). Used both for
    /// the BCC prologue check and for memory-model detection.
    pub first_ledata: Vec<u8>,
    /// Location-type field from each FIXUP locator byte seen
    /// (bits 5..2 of the Locat high byte). Type 1 = near 16-bit
    /// offset, type 3 = far 32-bit pointer. The mix distinguishes
    /// memory models for translator-stripped OBJs.
    pub fixup_location_types: Vec<u8>,
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
                if self.first_ledata.is_empty() && rec.payload.len() > 3 {
                    self.first_ledata = rec.payload[3..].to_vec();
                }
            }
            obj::FIXUPP_16 => {
                self.absorb_fixupp(rec.payload);
            }
            _ => {}
        }
    }

    /// Walk one FIXUPP record's payload and harvest each FIXUP
    /// subrecord's location-type field. THREAD subrecords are
    /// ignored (BCC doesn't use them and we don't need to model
    /// them for fingerprinting). Anything malformed terminates the
    /// scan early; we never panic.
    fn absorb_fixupp(&mut self, payload: &[u8]) {
        let mut i = 0;
        while i < payload.len() {
            let b0 = payload[i];
            if b0 & 0x80 == 0 {
                // THREAD subrecord: 1 byte of method-data, possibly
                // followed by an index byte. We don't model these.
                // For robustness, break rather than try to skip.
                break;
            }
            // FIXUP subrecord. Bits 5..2 of byte 0 are location type.
            let location = (b0 >> 2) & 0x0F;
            self.fixup_location_types.push(location);
            // Skip past Locat (2 bytes) and Fix Data (1 byte).
            if i + 2 >= payload.len() {
                break;
            }
            let fix_data = payload[i + 2];
            let mut consumed = 3;
            // Frame method (bits 6..4 of fix_data) takes 1 byte if
            // F=0 (explicit frame) and the method is 0/1/2.
            let f_flag = (fix_data & 0x80) != 0;
            let frame_method = (fix_data >> 4) & 0x07;
            if !f_flag && matches!(frame_method, 0 | 1 | 2) {
                consumed += 1;
            }
            // Target method (P + bits 1..0 of fix_data) always takes
            // 1 byte of datum (whether or not displacement follows).
            let t_flag = (fix_data & 0x08) != 0;
            let p_flag = (fix_data & 0x04) != 0;
            let target_low = fix_data & 0x03;
            if !t_flag {
                consumed += 1; // target datum
                if !p_flag {
                    // Methods 0/1/2 carry a 16-bit displacement.
                    if target_low <= 2 {
                        consumed += 2;
                    }
                }
            }
            i += consumed;
        }
    }

    /// LNAMES is a prefix of BCC's canonical 8-name sequence. The
    /// full list ends in `_BSS, BSS, DGROUP`; BCC drops these tail
    /// entries when the module needs no DGROUP (huge model) or no
    /// BSS segment, so a strict equality check would miss those.
    /// Requires at least 5 names (`"", _TEXT, CODE, _DATA, DATA`),
    /// the minimum BCC ever emits, and rejects multi-LNAMES inputs
    /// (a MASM/TASM-direct fingerprint).
    #[must_use]
    pub fn matches_bcc_lnames(&self) -> bool {
        if self.multi_lnames {
            return false;
        }
        match &self.first_lnames {
            Some(names) => {
                names.len() >= 5
                    && names.len() <= BCC_LNAMES.len()
                    && names.iter().zip(BCC_LNAMES.iter()).all(|(a, b)| a == b)
            }
            None => false,
        }
    }

    /// True when LNAMES doesn't include the `DGROUP` tail name. That
    /// happens in BCC's huge model, which has no DGROUP at all.
    fn lnames_lacks_dgroup(&self) -> bool {
        match &self.first_lnames {
            Some(names) => !names.iter().any(|n| n == "DGROUP"),
            None => true,
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
        self.first_ledata.starts_with(&[0x55, 0x8B, 0xEC])
    }

    /// First 16 bytes of the first LEDATA, suitable for verbose
    /// CLI output. Returns a slice rather than a copy.
    #[must_use]
    pub fn first_code_bytes(&self) -> &[u8] {
        let n = self.first_ledata.len().min(16);
        &self.first_ledata[..n]
    }

    /// Whether any FIXUP subrecord in this module used location
    /// type 3 (32-bit pointer / far reference). Strong signal that
    /// the module uses far code or far data.
    #[must_use]
    pub fn has_far_fixup(&self) -> bool {
        self.fixup_location_types.iter().any(|&t| t == 3)
    }

    /// Detect the memory model from code-distance signals in the
    /// first non-empty LEDATA payload. Returns `Unknown` when no
    /// `5d c3` or `5d cb` epilogue is found.
    #[must_use]
    pub fn memory_model(&self) -> MemoryModel {
        if self.first_ledata.is_empty() {
            return MemoryModel::Unknown;
        }
        let huge_setup = has_huge_ds_setup(&self.first_ledata);
        let code_far = scan_code_distance(&self.first_ledata);
        // Data distance: heuristic on FIXUPP locations. Far data
        // references would use location type 3. We saw earlier that
        // even compact-model OBJs use location 1 for the C-runtime
        // globals (DGROUP via near offset), so this isn't 100%
        // reliable — but combined with code distance it gives a
        // strong tier.
        let data_far = self.has_far_fixup() && !code_far;
        // Strictly: location 3 = 32-bit ptr. If code is near, the
        // only source of a 3-location fixup is a far data ref →
        // Compact. If code is far, far fixups conflate code and
        // data; we conservatively classify as Large when data
        // FIXUPPs are present without huge-style DS setup.
        match (code_far, data_far, huge_setup) {
            (true, _, true) => MemoryModel::Huge,
            (true, _, false) => {
                // Medium or Large. Tell them apart by whether any
                // _data_ fixup uses 32-bit ptr (large) vs 16-bit
                // offset (medium). We approximate: if there's any
                // FIXUP location 3 that's a self-relative call
                // (already counted in code_far) we can't tell. So
                // we check whether multiple distinct location
                // types appear — code-only-far → medium; both →
                // large.
                if self
                    .fixup_location_types
                    .iter()
                    .any(|&t| t == 1)
                {
                    MemoryModel::Medium
                } else {
                    MemoryModel::Large
                }
            }
            (false, true, _) => MemoryModel::Compact,
            (false, false, _) => MemoryModel::Small,
        }
    }
}

/// Return true if the first LEDATA bytes look like BCC's huge-model
/// DS-zero setup: after a `55 8B EC` prologue, the next 7 bytes are
/// `1E B8 00 00 8E D8` (push ds / mov ax,0 / mov ds,ax — used in
/// huge model to normalize pointer arithmetic).
fn has_huge_ds_setup(bytes: &[u8]) -> bool {
    bytes
        .windows(9)
        .any(|w| w == [0x55, 0x8B, 0xEC, 0x1E, 0xB8, 0x00, 0x00, 0x8E, 0xD8])
}

/// Scan a LEDATA payload for the function epilogue and decide
/// whether the code is near (`5d c3` = pop bp / ret near) or far
/// (`5d cb` = pop bp / ret far). Returns true when a far epilogue
/// is found; false otherwise (including when no epilogue is
/// found — caller should treat that as the default near case).
fn scan_code_distance(bytes: &[u8]) -> bool {
    // Look for pop bp + ret/retf. ret far (cb) is the far signal.
    for w in bytes.windows(2) {
        if w[0] == 0x5D {
            if w[1] == 0xCB {
                return true; // far
            }
            if w[1] == 0xC3 {
                return false; // near
            }
        }
    }
    // No epilogue found in this LEDATA; fall back to scanning for
    // far-call opcode 9A which only appears in far-code models.
    bytes.iter().any(|&b| b == 0x9A) && !bytes.iter().any(|&b| b == 0xE8)
}

/// Memory model targeted by an OBJ. Tiny is not separately
/// detected today (it shares its code-distance signal with Small);
/// most real-world OBJs are Small, Medium, Compact, Large, or Huge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryModel {
    Small,
    Medium,
    Compact,
    Large,
    Huge,
    Unknown,
}

impl MemoryModel {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Small => "small (-ms)",
            Self::Medium => "medium (-mm)",
            Self::Compact => "compact (-mc)",
            Self::Large => "large (-ml)",
            Self::Huge => "huge (-mh)",
            Self::Unknown => "unknown",
        }
    }
    pub fn slug(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Compact => "compact",
            Self::Large => "large",
            Self::Huge => "huge",
            Self::Unknown => "unknown",
        }
    }
}

impl ObjAnalysis {
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
    /// BCC-compiled.
    #[must_use]
    pub fn tier(&self) -> FingerprintTier {
        if self.has_bcc_translator() {
            return FingerprintTier::Bcc2_0Direct;
        }
        if self.matches_bcc_lnames() {
            // Huge-model OBJs omit DGROUP and GRPDEF — fall back to
            // the BCC prologue + huge DS-zero idiom as the corroborating
            // signal. Other models always emit BCC's standard GRPDEF.
            if self.matches_bcc_grpdef() {
                return FingerprintTier::Bcc2_0Stripped;
            }
            if self.lnames_lacks_dgroup() && self.has_bcc_prologue() {
                return FingerprintTier::Bcc2_0Stripped;
            }
        }
        if self.a1_marker_empty {
            return FingerprintTier::BorlandToolchainAsm;
        }
        FingerprintTier::Unknown
    }
}
