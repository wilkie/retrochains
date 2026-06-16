//! Generalization guard: the idiom catalog is mined from our own fixtures, so
//! this checks it still recognizes an *independent* real Borland C++ 2.0
//! program — JETPACK.EXE, a freeware DOS game (TLINK 4.0 linked: `e_csum=0`,
//! `e_lfarlc=0x3e`, the TLINK signature). It reads the EXE out of the gitignored
//! `jetpak15.zip` at the repo root and skips when that isn't present (the binary
//! isn't redistributed; provenance is the author's site, see the commit log).
//!
//! It guards two things a catalog change could regress: that real BCC code still
//! classifies as BCC (now that `classify` uses a dominance margin — the game's
//! ~61 BCC idioms vs a couple of coincidental MSC matches), and that idiom
//! coverage over the load image stays high.

use std::io::{Cursor, Read};
use std::path::PathBuf;

/// Decompress `jetpak15/JETPACK.EXE` from the repo-root zip, or `None` if the
/// zip isn't there.
fn jetpack_exe() -> Option<Vec<u8>> {
    let mut zip_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    zip_path.pop();
    zip_path.pop();
    zip_path.push("jetpak15.zip");
    let bytes = std::fs::read(&zip_path).ok()?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut file = archive.by_name("jetpak15/JETPACK.EXE").ok()?;
    let mut out = Vec::new();
    file.read_to_end(&mut out).ok()?;
    Some(out)
}

#[test]
fn jetpack_classifies_as_bcc_with_high_coverage() {
    let Some(exe) = jetpack_exe() else {
        eprintln!("skipping: jetpak15.zip not present at the repo root");
        return;
    };
    // Skip the MZ header (e_cparhdr paragraphs) to reach the load image, whose
    // first segments are the C0 startup and `_TEXT`.
    let header = (usize::from(exe[8]) | (usize::from(exe[9]) << 8)) * 16;
    let image = &exe[header..];
    let window = &image[..image.len().min(0x10000)];

    let class = fingerprint::classify(window);
    assert_eq!(
        class.verdict,
        fingerprint::Verdict::Bcc,
        "a real BCC game must classify as BCC (bcc-idioms={}, msc-idioms={})",
        class.bcc_evidence,
        class.msc_evidence,
    );

    // Mixed code+data; the catalog recognized ~70% when last measured. Guard a
    // floor so a catalog change can't quietly stop generalizing.
    let coverage = fingerprint::idioms::coverage(window);
    assert!(
        coverage >= 0.68,
        "idiom coverage on real BCC code regressed: {:.1}%",
        coverage * 100.0,
    );
}
