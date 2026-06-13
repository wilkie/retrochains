//! Fixture corpus loader and capture/verify harness. See `specs/FIXTURES.md`
//! for the on-disk format and the workflow this crate supports.

pub mod dashboard;
mod diff;
mod fixture;
mod harness;
mod manifest;
mod timefmt;

pub use diff::{Diff, FileDiff, FileDiffKind, ManifestDiff};
pub use fixture::{Fixture, Invocation, LoadError, ToolName};
pub use harness::{HarnessError, ToolPaths, capture, verify_oracle, verify_ours};
pub use manifest::{Manifest, OracleSummary, OutputEntry, RunSummary};

use std::path::{Path, PathBuf};

/// Recursively collect every fixture directory under `root`, i.e. every dir
/// containing an `invocation.*.toml`. Fixtures may live at any depth, so the
/// corpus can be organized by language and category — `fixtures/c/<name>/`,
/// `fixtures/c/<category>/<name>/`, `fixtures/cpp/<category>/<name>/`, etc.
/// A directory that IS a fixture is collected and not descended into (its
/// `expected/` tree never holds an invocation file). When `compiler` is
/// `Some(name)`, only fixtures targeting that compiler (`invocation.<name>.toml`)
/// are returned; `None` returns any fixture. Results are sorted for determinism.
pub fn discover_fixtures(root: &Path, compiler: Option<&str>) -> std::io::Result<Vec<PathBuf>> {
    let has_invocation = |dir: &Path| -> bool {
        match compiler {
            Some(name) => dir.join(format!("invocation.{name}.toml")).is_file(),
            None => std::fs::read_dir(dir).is_ok_and(|rd| {
                rd.filter_map(Result::ok).any(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("invocation.") && n.ends_with(".toml"))
                })
            }),
        }
    };
    fn walk(dir: &Path, is_fixture: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        if is_fixture(dir) {
            out.push(dir.to_path_buf());
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), is_fixture, out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, &has_invocation, &mut out)?;
    out.sort();
    Ok(out)
}
