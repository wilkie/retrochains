//! WASM bindings for the **compiler-free decompiler** plus the **fingerprint**
//! analyzer, exposed to the `@retrochains/decompile` TypeScript package.
//!
//! Two directions over one module:
//!   - *analysis* — `classify` (which compiler made these bytes), `coverage`
//!     (fraction recognized as known idioms), `code_of_obj` (pull `_TEXT` out of
//!     an OMF object);
//!   - *decompilation* — `decompile` / `decompile_program` (machine code →
//!     compiler-accurate C).
//!
//! `decompile` is linked WITHOUT its `bcc` feature, so this module bundles **no
//! compiler** (~50 KB gzipped, vs ~1.5 MB if it shipped one to verify). Round-trip
//! verification is therefore an app-level step: classify → decompile → recompile
//! with whichever compiler module (`@retrochains/bcc`) the verdict points at.
//!
//! Like the other `*-wasm` crates, the whole crate is `#![cfg(target_arch =
//! "wasm32")]` with target-gated deps. Build with
//! `scripts/build-wasm.sh decompile-wasm decompile decompile_wasm`.
#![cfg(target_arch = "wasm32")]
// wasm-bindgen's macro expands `unsafe` glue; the workspace denies `unsafe_code`.
#![allow(unsafe_code)]
// Evidence tallies are small counts; the `usize`→`u32` casts can't realistically
// truncate for any real `_TEXT`.
#![allow(clippy::cast_possible_truncation)]

use wasm_bindgen::prelude::*;

// ── analysis (fingerprint) ──────────────────────────────────────────────────

/// The verdict of [`classify`]: which compiler the code idioms point at, plus the
/// distinctive-idiom tallies it rests on.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct Classification {
    verdict: String,
    bcc_evidence: u32,
    msc_evidence: u32,
    idiom_count: u32,
}

#[wasm_bindgen]
impl Classification {
    /// `"bcc"`, `"msc"`, `"ambiguous"`, or `"unknown"`.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn verdict(&self) -> String {
        self.verdict.clone()
    }

    /// Count of BCC-distinctive idiom hits.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn bcc_evidence(&self) -> u32 {
        self.bcc_evidence
    }

    /// Count of MSC-distinctive idiom hits.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn msc_evidence(&self) -> u32 {
        self.msc_evidence
    }

    /// Total recognized idioms in the decomposition.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn idiom_count(&self) -> u32 {
        self.idiom_count
    }
}

/// Decide, from `_TEXT` machine-code idioms alone, which compiler produced it.
#[wasm_bindgen]
#[must_use]
pub fn classify(code: &[u8]) -> Classification {
    let c = fingerprint::classify(code);
    let verdict = match c.verdict {
        fingerprint::Verdict::Bcc => "bcc",
        fingerprint::Verdict::Msc => "msc",
        fingerprint::Verdict::Ambiguous => "ambiguous",
        fingerprint::Verdict::Unknown => "unknown",
    };
    Classification {
        verdict: verdict.to_string(),
        bcc_evidence: c.bcc_evidence as u32,
        msc_evidence: c.msc_evidence as u32,
        idiom_count: c.matches.len() as u32,
    }
}

/// Fraction of `code` bytes that lift to a recognized BCC/MSC idiom (0.0–1.0).
#[wasm_bindgen]
#[must_use]
pub fn coverage(code: &[u8]) -> f32 {
    fingerprint::idioms::coverage(code)
}

/// Pull the first CODE-class segment (`_TEXT`) out of an OMF object's bytes — the
/// input the decompiler and `classify` want, given a raw `.OBJ`.
#[wasm_bindgen]
#[must_use]
pub fn code_of_obj(obj: &[u8]) -> Vec<u8> {
    fingerprint::idioms::code_of_obj(obj)
}

// ── decompilation ───────────────────────────────────────────────────────────

/// Decompile a single function's `_TEXT` to compiler-accurate C, or `undefined`
/// if it isn't fully recovered. Form-neutral (no recompile oracle here): verify
/// it by recompiling with the matching compiler module.
#[wasm_bindgen]
#[must_use]
pub fn decompile(code: &[u8]) -> Option<String> {
    decompile::decompile(code)
}

/// Decompile a whole `_TEXT` segment (splitting it into functions at the
/// prologues), or `undefined` if any function isn't fully recovered.
#[wasm_bindgen]
#[must_use]
pub fn decompile_program(code: &[u8]) -> Option<String> {
    decompile::decompile_program(code)
}
