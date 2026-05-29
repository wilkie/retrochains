//! `xfix` — drive the fixture corpus from the shell. Three subcommands:
//!
//!     xfix capture <fixture>                       # run the oracle, write expected/
//!     xfix verify [--toolchain T] <fixture>        # diff a fresh run against expected/
//!     xfix verify-all [--toolchain T] [--jobs N]   # verify every fixture in parallel
//!
//! `--toolchain oracle` (default) re-runs the oracle (a determinism check
//! on the capture itself). `--toolchain ours` runs our host-side
//! reimplementation (e.g. `target/debug/bcc`) and is the path tests use.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use fixtures::{Diff, FileDiffKind, Fixture, ManifestDiff, ToolPaths, capture, verify_oracle, verify_ours};

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xfix: {e}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Toolchain {
    Oracle,
    Ours,
}

fn try_main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut it = argv.iter();
    let sub = it
        .next()
        .ok_or("usage: xfix <capture|verify|verify-all> [flags] [<fixture>]")?;

    let mut toolchain = Toolchain::Oracle;
    let mut fixture_path: Option<PathBuf> = None;
    let mut jobs: Option<usize> = None;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--toolchain" => {
                let v = it.next().ok_or("--toolchain needs a value (oracle|ours)")?;
                toolchain = match v.as_str() {
                    "oracle" => Toolchain::Oracle,
                    "ours" => Toolchain::Ours,
                    other => return Err(format!("unknown toolchain: {other}").into()),
                };
            }
            "--jobs" => {
                let v = it.next().ok_or("--jobs needs a positive integer")?;
                jobs = Some(v.parse().map_err(|_| format!("--jobs: not a number: {v}"))?);
            }
            path if !path.starts_with("--") => {
                fixture_path = Some(PathBuf::from(path));
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
    }

    let workspace_root = find_workspace_root()?;

    match sub.as_str() {
        "capture" => {
            let fixture_path = fixture_path.ok_or("missing <fixture> path")?;
            let fixture = Fixture::load(&fixture_path)?;
            capture(&workspace_root, &fixture)?;
            eprintln!("[xfix] captured {}", fixture.name);
            Ok(ExitCode::from(0))
        }
        "verify" => {
            let fixture_path = fixture_path.ok_or("missing <fixture> path")?;
            let fixture = Fixture::load(&fixture_path)?;
            let diff = match toolchain {
                Toolchain::Oracle => verify_oracle(&workspace_root, &fixture)?,
                Toolchain::Ours => {
                    let tool_paths = ToolPaths::from_workspace_debug(&workspace_root);
                    verify_ours(&fixture, &tool_paths)?
                }
            };
            print_diff(&fixture.name, &diff);
            if diff.is_empty() {
                Ok(ExitCode::from(0))
            } else {
                Ok(ExitCode::from(1))
            }
        }
        "verify-all" => verify_all(&workspace_root, toolchain, jobs),
        other => Err(format!("unknown subcommand: {other}").into()),
    }
}

/// Walk every fixture directory under `<workspace>/fixtures/` and run
/// the verify path in parallel. Reports pass/fail counts and lists
/// failing fixtures. Exit 0 only when every fixture matches.
fn verify_all(
    workspace_root: &Path,
    toolchain: Toolchain,
    jobs: Option<usize>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(workspace_root.join("fixtures"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| p.join("invocation.toml").is_file())
        .collect();
    paths.sort();
    let total = paths.len();

    let num_threads = jobs
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(8)
        .max(1);

    let tool_paths = ToolPaths::from_workspace_debug(workspace_root);
    let pass = AtomicUsize::new(0);
    let fail = AtomicUsize::new(0);
    let failures: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    let start = std::time::Instant::now();
    let chunk_size = total.div_ceil(num_threads).max(1);

    std::thread::scope(|s| {
        for chunk in paths.chunks(chunk_size) {
            let tool_paths = &tool_paths;
            let pass = &pass;
            let fail = &fail;
            let failures = &failures;
            s.spawn(move || {
                for path in chunk {
                    let result = run_one(path, toolchain, workspace_root, tool_paths);
                    match result {
                        Ok((name, diff)) => {
                            if diff.is_empty() {
                                pass.fetch_add(1, Ordering::Relaxed);
                            } else {
                                fail.fetch_add(1, Ordering::Relaxed);
                                failures
                                    .lock()
                                    .expect("failures mutex poisoned")
                                    .push((name, summarize_diff(&diff)));
                            }
                        }
                        Err((name, e)) => {
                            fail.fetch_add(1, Ordering::Relaxed);
                            failures
                                .lock()
                                .expect("failures mutex poisoned")
                                .push((name, format!("error: {e}")));
                        }
                    }
                }
            });
        }
    });

    let elapsed = start.elapsed();
    let pass = pass.load(Ordering::Relaxed);
    let fail = fail.load(Ordering::Relaxed);
    let mut failures = failures.into_inner().expect("failures mutex poisoned");
    failures.sort_by(|a, b| a.0.cmp(&b.0));

    eprintln!(
        "[xfix] verified {total} fixtures in {:.1}s ({num_threads} threads): {pass} pass, {fail} fail",
        elapsed.as_secs_f64(),
    );
    for (name, msg) in &failures {
        eprintln!("  FAIL {name}: {msg}");
    }
    if fail == 0 { Ok(ExitCode::from(0)) } else { Ok(ExitCode::from(1)) }
}

fn run_one(
    path: &Path,
    toolchain: Toolchain,
    workspace_root: &Path,
    tool_paths: &ToolPaths,
) -> Result<(String, Diff), (String, String)> {
    let name_fallback = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>")
        .to_owned();
    let fixture =
        Fixture::load(path).map_err(|e| (name_fallback.clone(), e.to_string()))?;
    let name = fixture.name.clone();
    let diff = match toolchain {
        Toolchain::Oracle => verify_oracle(workspace_root, &fixture),
        Toolchain::Ours => verify_ours(&fixture, tool_paths),
    }
    .map_err(|e| (name.clone(), e.to_string()))?;
    Ok((name, diff))
}

/// One-line summary of a mismatch — picks the first concrete failure
/// so the parallel summary stays readable when many fixtures fail.
fn summarize_diff(diff: &Diff) -> String {
    if let Some(m) = diff.manifest.first() {
        return match m {
            ManifestDiff::ExitCode { expected, actual } => {
                format!("exit_code {expected}→{actual}")
            }
            ManifestDiff::StdoutSha { .. } => "stdout sha differs".to_owned(),
            ManifestDiff::StderrSha { .. } => "stderr sha differs".to_owned(),
            ManifestDiff::OutputMissing { name } => format!("missing output {name}"),
            ManifestDiff::OutputUnexpected { name } => format!("unexpected output {name}"),
            ManifestDiff::OutputMetadata { name, field, expected, actual } => {
                format!("{name}.{field} {expected}→{actual}")
            }
        };
    }
    if let Some(f) = diff.files.first() {
        return match &f.kind {
            FileDiffKind::Length { expected, actual } => {
                format!("{} length {expected}→{actual}", f.name)
            }
            FileDiffKind::Bytes { first_diff_offset, .. } => {
                format!("{} differs at offset {first_diff_offset}", f.name)
            }
        };
    }
    "mismatch (no detail)".to_owned()
}

fn print_diff(fixture_name: &str, diff: &Diff) {
    if !diff.has_any() {
        eprintln!("[xfix] {fixture_name}: match");
        return;
    }
    if diff.is_empty() {
        eprintln!("[xfix] {fixture_name}: match (advisory differences below)");
    } else {
        eprintln!("[xfix] {fixture_name}: MISMATCH");
    }
    for m in &diff.manifest {
        match m {
            ManifestDiff::ExitCode { expected, actual } => {
                eprintln!("  exit_code: expected {expected}, got {actual}");
            }
            ManifestDiff::StdoutSha { expected, actual } => {
                eprintln!("  stdout_sha256: {expected} -> {actual}");
            }
            ManifestDiff::StderrSha { expected, actual } => {
                eprintln!("  stderr_sha256: {expected} -> {actual}");
            }
            ManifestDiff::OutputMissing { name } => {
                eprintln!("  output missing: {name}");
            }
            ManifestDiff::OutputUnexpected { name } => {
                eprintln!("  unexpected output: {name}");
            }
            ManifestDiff::OutputMetadata { name, field, expected, actual } => {
                eprintln!("  {name}.{field}: {expected} -> {actual}");
            }
        }
    }
    for f in &diff.files {
        print_file_diff("", f);
    }
    for f in &diff.advisory {
        print_file_diff("[advisory] ", f);
    }
}

fn print_file_diff(prefix: &str, f: &FileDiffKindCarrier) {
    match &f.kind {
        FileDiffKind::Length { expected, actual } => {
            eprintln!("  {prefix}{}: length {} -> {}", f.name, expected, actual);
        }
        FileDiffKind::Bytes { first_diff_offset, expected_window, actual_window } => {
            eprintln!(
                "  {prefix}{}: first byte differs at offset {first_diff_offset}",
                f.name
            );
            eprintln!("    expected: {}", hex_window(expected_window));
            eprintln!("    actual:   {}", hex_window(actual_window));
        }
    }
}

// Type alias so the helper above stays decoupled from the exact name we
// happened to pick in `diff.rs`.
type FileDiffKindCarrier = fixtures::FileDiff;

fn hex_window(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn find_workspace_root() -> std::io::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let mut dir: &Path = cwd.as_path();
    loop {
        if dir.join("BC2.zip").is_file() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(cwd),
        }
    }
}
