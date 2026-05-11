//! `xfix` — drive the fixture corpus from the shell. Two subcommands:
//!
//!     xfix capture <fixture>     # run the oracle, write expected/
//!     xfix verify <fixture>      # re-run the oracle, diff against expected/

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fixtures::{Diff, FileDiffKind, Fixture, ManifestDiff, capture, verify_oracle};

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xfix: {e}");
            ExitCode::from(2)
        }
    }
}

fn try_main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut argv = std::env::args().skip(1);
    let sub = argv.next().ok_or("usage: xfix <capture|verify> <fixture>")?;
    let fixture_path: PathBuf = argv
        .next()
        .ok_or("missing <fixture> path")?
        .into();

    let workspace_root = find_workspace_root()?;
    let fixture = Fixture::load(&fixture_path)?;

    match sub.as_str() {
        "capture" => {
            capture(&workspace_root, &fixture)?;
            eprintln!("[xfix] captured {}", fixture.name);
            Ok(ExitCode::from(0))
        }
        "verify" => {
            let diff = verify_oracle(&workspace_root, &fixture)?;
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
    if diff.is_empty() {
        eprintln!("[xfix] {fixture_name}: match");
        return;
    }
    eprintln!("[xfix] {fixture_name}: MISMATCH");
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
        match &f.kind {
            FileDiffKind::Length { expected, actual } => {
                eprintln!("  {}: length {} -> {}", f.name, expected, actual);
            }
            FileDiffKind::Bytes { first_diff_offset, expected_window, actual_window } => {
                eprintln!(
                    "  {}: first byte differs at offset {first_diff_offset}",
                    f.name
                );
                eprintln!("    expected: {}", hex_window(expected_window));
                eprintln!("    actual:   {}", hex_window(actual_window));
            }
        }
    }
}

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
