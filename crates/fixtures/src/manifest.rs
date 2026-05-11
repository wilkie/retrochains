//! The `expected/manifest.toml` schema. Acts as a fast-scan summary of the
//! goldens (sizes, hashes, mtimes) and a self-describing record of how the
//! oracle was invoked.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub run: RunSummary,
    /// Recorded sorted by `name` for stable diffs.
    pub outputs: Vec<OutputEntry>,
    pub oracle: OracleSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunSummary {
    pub exit_code: i32,
    /// Hex-encoded SHA-256 of captured stdout. Lets us notice changes in
    /// human-readable streams without diffing the bytes.
    pub stdout_sha256: String,
    /// Hex-encoded SHA-256 of captured stderr. Often empty (DOSBox 0.74's
    /// shell may not split stderr from stdout).
    pub stderr_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputEntry {
    /// DOS filename (uppercased) as the tool wrote it.
    pub name: String,
    /// Byte length of the file's contents.
    pub size: u64,
    /// Hex-encoded SHA-256 of the file's contents.
    pub sha256: String,
    /// RFC3339 UTC timestamp of the file's mtime as the tool left it, or
    /// `None` if the host filesystem couldn't report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OracleSummary {
    /// Tool name (`bcc` / `tasm` / `tlink`).
    pub tool: String,
    /// Argument vector passed to the tool. Filenames appear here as the
    /// DOS-uppercase names materialized in the work directory.
    pub args: Vec<String>,
    /// Versions and configuration the capture was taken under. Useful for
    /// debugging drift in the oracle itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dosbox_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fake_time: Option<String>,
}
