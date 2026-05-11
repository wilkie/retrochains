//! The capture/verify harness. `capture` drives the oracle and writes
//! goldens; `verify_oracle` re-drives the oracle and diffs against goldens
//! (a deterministic-by-construction sanity check that also proves the
//! capture is reproducible).

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use oracle::{Oracle, OracleConfig, OracleInvocation, OracleRun};

use crate::diff::{Diff, diff_bytes, diff_manifests};
use crate::fixture::Fixture;
use crate::manifest::{Manifest, OracleSummary, OutputEntry, RunSummary};
use crate::timefmt;

/// Capture a fixture: run the oracle and write a fresh `expected/`.
///
/// # Errors
/// Returns [`HarnessError`] if the oracle fails to open or run, or the
/// fixture's inputs / expected directory can't be (re)written.
pub fn capture(workspace_root: &Path, fixture: &Fixture) -> Result<(), HarnessError> {
    let run = run_oracle(workspace_root, fixture)?;
    let manifest = build_manifest(fixture, &run, oracle_summary(fixture));

    let expected = fixture.expected_dir();
    if expected.exists() {
        fs::remove_dir_all(&expected)?;
    }
    fs::create_dir_all(&expected)?;

    fs::write(expected.join("stdout"), &run.stdout)?;
    fs::write(expected.join("stderr"), &run.stderr)?;
    for output in &run.outputs {
        fs::write(expected.join(output.0), &output.1.bytes)?;
    }
    let manifest_text = toml::to_string_pretty(&manifest)
        .map_err(|e| HarnessError::ManifestSerialize(e.to_string()))?;
    fs::write(expected.join("manifest.toml"), manifest_text)?;
    Ok(())
}

/// Verify by re-running the **oracle** against the fixture. Useful as a
/// determinism check on the oracle itself — two captures of the same
/// fixture must produce identical goldens.
///
/// # Errors
/// Returns [`HarnessError`] if the oracle fails, or if the fixture has no
/// `expected/` to compare against.
pub fn verify_oracle(workspace_root: &Path, fixture: &Fixture) -> Result<Diff, HarnessError> {
    let expected_manifest = load_expected_manifest(fixture)?;
    let run = run_oracle(workspace_root, fixture)?;
    let actual_manifest = build_manifest(fixture, &run, oracle_summary(fixture));

    let mut diff = Diff {
        manifest: diff_manifests(&expected_manifest, &actual_manifest),
        ..Diff::default()
    };

    let expected_dir = fixture.expected_dir();
    if let Some(file_diff) = diff_bytes("stdout", &read_file(&expected_dir, "stdout")?, &run.stdout)
    {
        diff.files.push(file_diff);
    }
    if let Some(file_diff) = diff_bytes("stderr", &read_file(&expected_dir, "stderr")?, &run.stderr)
    {
        diff.files.push(file_diff);
    }
    for (name, output) in &run.outputs {
        let expected = read_file_opt(&expected_dir, name)?;
        if let Some(expected) = expected
            && let Some(file_diff) = diff_bytes(name, &expected, &output.bytes)
        {
            diff.files.push(file_diff);
        }
    }
    Ok(diff)
}

fn run_oracle(workspace_root: &Path, fixture: &Fixture) -> Result<OracleRun, HarnessError> {
    let cfg = OracleConfig::for_workspace(workspace_root);
    let inputs = fixture.load_inputs()?;
    let oracle = Oracle::open(cfg).map_err(|e| HarnessError::Oracle(e.to_string()))?;
    let mut inv = OracleInvocation::new(fixture.invocation.tool.as_oracle())
        .args(fixture.invocation.args.clone());
    for (name, bytes) in &inputs {
        inv = inv.input(name.clone(), bytes.as_slice());
    }
    oracle.run(&inv).map_err(|e| HarnessError::Oracle(e.to_string()))
}

fn build_manifest(fixture: &Fixture, run: &OracleRun, oracle: OracleSummary) -> Manifest {
    let mut outputs: Vec<OutputEntry> = run
        .outputs
        .iter()
        .map(|(name, out)| OutputEntry {
            name: name.clone(),
            size: out.bytes.len() as u64,
            sha256: sha256_hex(&out.bytes),
            mtime: out.mtime.and_then(timefmt::format),
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    let _ = fixture; // suppress unused warning until oracle summary uses fixture
    Manifest {
        run: RunSummary {
            exit_code: run.exit_code,
            stdout_sha256: sha256_hex(&run.stdout),
            stderr_sha256: sha256_hex(&run.stderr),
        },
        outputs,
        oracle,
    }
}

fn oracle_summary(fixture: &Fixture) -> OracleSummary {
    OracleSummary {
        tool: fixture.invocation.tool.as_str().to_owned(),
        args: fixture.invocation.args.clone(),
        dosbox_version: None,
        fake_time: Some("1991-04-23T12:00:00Z".to_owned()),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(64);
    for byte in digest {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

fn load_expected_manifest(fixture: &Fixture) -> Result<Manifest, HarnessError> {
    let path = fixture.expected_dir().join("manifest.toml");
    let text = fs::read_to_string(&path)
        .map_err(|e| HarnessError::MissingExpected(path.clone(), e.to_string()))?;
    toml::from_str(&text).map_err(|e| HarnessError::ManifestParse(path, e.to_string()))
}

fn read_file(dir: &Path, name: &str) -> Result<Vec<u8>, HarnessError> {
    fs::read(dir.join(name)).map_err(HarnessError::Io)
}

fn read_file_opt(dir: &Path, name: &str) -> Result<Option<Vec<u8>>, HarnessError> {
    let path = dir.join(name);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(fs::read(&path)?))
}

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("oracle: {0}")]
    Oracle(String),
    #[error("load: {0}")]
    Load(#[from] crate::fixture::LoadError),
    #[error("manifest serialize: {0}")]
    ManifestSerialize(String),
    #[error("manifest parse ({0}): {1}")]
    ManifestParse(PathBuf, String),
    #[error("no captured expected/ at {0}: {1}")]
    MissingExpected(PathBuf, String),
}

