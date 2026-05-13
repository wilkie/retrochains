//! Classify an OBJ or LIB by its toolchain fingerprint. Tier-1
//! detection looks for the TASM-2.51-injected COMENTs that BCC's
//! `-c` flow emits (translator string, model marker, trailing
//! debug record); tier-2 falls back on structural signals that
//! survive TLIB stripping (LNAMES order, SEGDEF ACBP bytes,
//! GRPDEF layout, codegen idioms).
//!
//! See `specs/formats/LIB_ARCHIVE.md` for the empirical basis of
//! the tier-2 signals, and `specs/FINGERPRINTS.md` for the full
//! catalog of patterns.

use obj::ObjReader;

mod lib_archive;
mod obj_analysis;

pub use lib_archive::{LibAnalysis, LibMember};
pub use obj_analysis::ObjAnalysis;

/// Result of inspecting one input file or byte buffer.
#[derive(Debug)]
pub enum Analysis {
    /// A standalone OMF object module (starts with THEADR, 0x80).
    Obj(ObjAnalysis),
    /// A Microsoft/Borland library archive (starts with 0xF0 header).
    Lib(LibAnalysis),
    /// We don't recognize this file as OBJ or LIB. The byte holds
    /// the first byte of the input for diagnostics.
    Unknown { first_byte: u8 },
}

impl Analysis {
    /// One-line headline (`Tier1 BCC 2.0`, `Mixed library`, etc.)
    /// suitable for the CLI's compact mode.
    #[must_use]
    pub fn headline(&self) -> String {
        match self {
            Self::Obj(o) => format!("OBJ: {}", o.tier().describe()),
            Self::Lib(l) => format!("LIB: {} members, {}", l.members.len(), l.summary_line()),
            Self::Unknown { first_byte } => {
                format!("unknown format (first byte 0x{first_byte:02x})")
            }
        }
    }
}

/// Toolchain classification for an individual OBJ. Ordered roughly
/// by specificity — Tier1 is the cheapest, most reliable check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FingerprintTier {
    /// TASM 2.51's translator-string COMENT is present, carrying
    /// the literal `TC86 Borland Turbo C++ 2.0`. This is BCC's
    /// most distinctive single byte sequence.
    Bcc2_0Direct,
    /// Translator COMENT stripped, but structural signals (LNAMES
    /// order, SEGDEF ACBPs, GRPDEF layout) match BCC's exact
    /// emission style. Most likely a BCC-compiled OBJ that passed
    /// through TLIB, which scrubs the identifying COMENTs.
    Bcc2_0Stripped,
    /// The empty-payload 0xA1 COMENT (TLIB archive marker) is
    /// present, but the structural shape diverges from BCC's
    /// (e.g., multi-LNAMES style). Most likely a Borland-archived
    /// hand-written ASM module.
    BorlandToolchainAsm,
    /// Nothing matched. Could be MASM, a different version, or
    /// some other vendor entirely.
    Unknown,
}

impl FingerprintTier {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Bcc2_0Direct => "BCC 2.0 (translator string present)",
            Self::Bcc2_0Stripped => "BCC 2.0 (translator stripped; structural match)",
            Self::BorlandToolchainAsm => "Borland-archived assembly",
            Self::Unknown => "unknown toolchain",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Bcc2_0Direct => "bcc20_direct",
            Self::Bcc2_0Stripped => "bcc20_stripped",
            Self::BorlandToolchainAsm => "borland_asm",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AnalyzeError {
    #[error("OMF read error: {0}")]
    Read(#[from] obj::ReadError),
    #[error("empty input")]
    Empty,
}

/// Inspect a byte buffer. Auto-detects OBJ vs LIB from the first
/// byte; falls back to `Unknown` if it doesn't match either.
///
/// # Errors
/// Returns [`AnalyzeError`] if OMF framing inside the file is
/// malformed (truncated record, etc.).
pub fn analyze(data: &[u8]) -> Result<Analysis, AnalyzeError> {
    let Some(&first) = data.first() else {
        return Err(AnalyzeError::Empty);
    };
    match first {
        obj::THEADR => Ok(Analysis::Obj(analyze_obj(data)?)),
        obj::LIBHDR => Ok(Analysis::Lib(lib_archive::analyze(data)?)),
        _ => Ok(Analysis::Unknown { first_byte: first }),
    }
}

/// Analyze a single OBJ stream. Public so the LIB walker can reuse
/// it per-member without going back through `analyze`.
///
/// # Errors
/// Returns [`AnalyzeError`] on a framing error inside the OBJ.
pub fn analyze_obj(data: &[u8]) -> Result<ObjAnalysis, AnalyzeError> {
    let mut reader = ObjReader::new(data);
    let mut info = ObjAnalysis::default();
    while let Some(rec) = reader.next()? {
        info.absorb(&rec);
        if rec.ty == obj::MODEND_16 {
            break;
        }
    }
    Ok(info)
}
