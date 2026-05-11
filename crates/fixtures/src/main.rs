//! `xfix` — drive the fixture corpus from the shell. Two subcommands:
//!
//!     xfix capture <fixture>                  # run the oracle, write expected/
//!     xfix verify [--toolchain T] <fixture>   # diff a fresh run against expected/
//!
//! `--toolchain oracle` (default) re-runs the oracle (a determinism check
//! on the capture itself). `--toolchain ours` runs our host-side
//! reimplementation (e.g. `target/debug/bcc`) and is the path tests use.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
        .ok_or("usage: xfix <capture|verify> [--toolchain T] <fixture>")?;

    let mut toolchain = Toolchain::Oracle;
    let mut fixture_path: Option<PathBuf> = None;
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
            path if !path.starts_with("--") => {
                fixture_path = Some(PathBuf::from(path));
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
    }
    let fixture_path = fixture_path.ok_or("missing <fixture> path")?;

    let workspace_root = find_workspace_root()?;
    let fixture = Fixture::load(&fixture_path)?;

    match sub.as_str() {
        "capture" => {
            capture(&workspace_root, &fixture)?;
            eprintln!("[xfix] captured {}", fixture.name);
            Ok(ExitCode::from(0))
        }
        "verify" => {
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
        other => Err(format!("unknown subcommand: {other}").into()),
    }
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
