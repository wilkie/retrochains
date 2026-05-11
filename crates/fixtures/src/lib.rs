//! Fixture corpus loader and capture/verify harness. See `specs/FIXTURES.md`
//! for the on-disk format and the workflow this crate supports.

mod diff;
mod fixture;
mod harness;
mod manifest;
mod timefmt;

pub use diff::{Diff, FileDiff, FileDiffKind, ManifestDiff};
pub use fixture::{Fixture, Invocation, LoadError, ToolName};
pub use harness::{HarnessError, ToolPaths, capture, verify_oracle, verify_ours};
pub use manifest::{Manifest, OracleSummary, OutputEntry, RunSummary};
