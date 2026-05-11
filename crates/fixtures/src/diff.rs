//! Diff types reported by `verify`. The harness produces a `Diff` from
//! comparing the actual run against the goldens; callers render it.

use crate::manifest::Manifest;

/// One fixture's verification report.
#[derive(Debug, Default)]
pub struct Diff {
    /// Manifest-level mismatches (fast scan). Gating.
    pub manifest: Vec<ManifestDiff>,
    /// Per-file content mismatches. Gating.
    pub files: Vec<FileDiff>,
    /// Mismatches that are reported but don't fail the run. Currently
    /// stdout/stderr under `verify_ours`, where a native Rust impl can't
    /// reproduce BCC's "Available memory N" banner.
    pub advisory: Vec<FileDiff>,
}

impl Diff {
    /// Whether the *gating* portion of the diff is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.manifest.is_empty() && self.files.is_empty()
    }

    /// Whether anything at all (gating or advisory) was reported.
    #[must_use]
    pub fn has_any(&self) -> bool {
        !self.manifest.is_empty() || !self.files.is_empty() || !self.advisory.is_empty()
    }
}

/// A discrepancy at the manifest level.
#[derive(Debug, Clone)]
pub enum ManifestDiff {
    ExitCode { expected: i32, actual: i32 },
    StdoutSha { expected: String, actual: String },
    StderrSha { expected: String, actual: String },
    /// A file was named in the manifest but not produced by the run.
    OutputMissing { name: String },
    /// The run produced a file not named in the manifest.
    OutputUnexpected { name: String },
    /// File appears in both but its size/sha/mtime differs.
    OutputMetadata {
        name: String,
        field: &'static str,
        expected: String,
        actual: String,
    },
}

/// A discrepancy in a file's bytes.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub name: String,
    pub kind: FileDiffKind,
}

#[derive(Debug, Clone)]
pub enum FileDiffKind {
    /// Lengths differ.
    Length { expected: usize, actual: usize },
    /// Same length, content differs. First differing byte is highlighted.
    Bytes {
        first_diff_offset: usize,
        expected_window: Vec<u8>,
        actual_window: Vec<u8>,
    },
}

/// Compare two manifests; pure on the input, no I/O.
#[must_use]
pub fn diff_manifests(expected: &Manifest, actual: &Manifest) -> Vec<ManifestDiff> {
    let mut diffs = Vec::new();
    if expected.run.exit_code != actual.run.exit_code {
        diffs.push(ManifestDiff::ExitCode {
            expected: expected.run.exit_code,
            actual: actual.run.exit_code,
        });
    }
    if expected.run.stdout_sha256 != actual.run.stdout_sha256 {
        diffs.push(ManifestDiff::StdoutSha {
            expected: expected.run.stdout_sha256.clone(),
            actual: actual.run.stdout_sha256.clone(),
        });
    }
    if expected.run.stderr_sha256 != actual.run.stderr_sha256 {
        diffs.push(ManifestDiff::StderrSha {
            expected: expected.run.stderr_sha256.clone(),
            actual: actual.run.stderr_sha256.clone(),
        });
    }
    let expected_names: std::collections::BTreeSet<&str> =
        expected.outputs.iter().map(|o| o.name.as_str()).collect();
    let actual_names: std::collections::BTreeSet<&str> =
        actual.outputs.iter().map(|o| o.name.as_str()).collect();
    for missing in expected_names.difference(&actual_names) {
        diffs.push(ManifestDiff::OutputMissing { name: (*missing).to_owned() });
    }
    for extra in actual_names.difference(&expected_names) {
        diffs.push(ManifestDiff::OutputUnexpected { name: (*extra).to_owned() });
    }
    for e in &expected.outputs {
        let Some(a) = actual.outputs.iter().find(|o| o.name == e.name) else { continue };
        if e.size != a.size {
            diffs.push(ManifestDiff::OutputMetadata {
                name: e.name.clone(),
                field: "size",
                expected: e.size.to_string(),
                actual: a.size.to_string(),
            });
        }
        if e.sha256 != a.sha256 {
            diffs.push(ManifestDiff::OutputMetadata {
                name: e.name.clone(),
                field: "sha256",
                expected: e.sha256.clone(),
                actual: a.sha256.clone(),
            });
        }
        if e.mtime != a.mtime {
            diffs.push(ManifestDiff::OutputMetadata {
                name: e.name.clone(),
                field: "mtime",
                expected: e.mtime.clone().unwrap_or_default(),
                actual: a.mtime.clone().unwrap_or_default(),
            });
        }
    }
    diffs
}

/// Sixteen-byte windows centered on the first differing byte (clamped).
const DIFF_WINDOW: usize = 16;

/// Compare two byte slices; produce a `FileDiff` if they differ.
#[must_use]
pub fn diff_bytes(name: &str, expected: &[u8], actual: &[u8]) -> Option<FileDiff> {
    if expected == actual {
        return None;
    }
    if expected.len() != actual.len() {
        return Some(FileDiff {
            name: name.to_owned(),
            kind: FileDiffKind::Length {
                expected: expected.len(),
                actual: actual.len(),
            },
        });
    }
    let first_diff = expected
        .iter()
        .zip(actual.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(0);
    let start = first_diff.saturating_sub(DIFF_WINDOW / 2);
    let end = (first_diff + DIFF_WINDOW / 2).min(expected.len());
    Some(FileDiff {
        name: name.to_owned(),
        kind: FileDiffKind::Bytes {
            first_diff_offset: first_diff,
            expected_window: expected[start..end].to_vec(),
            actual_window: actual[start..end].to_vec(),
        },
    })
}
