//! The capture/verify harness. `capture` drives the oracle and writes
//! goldens; `verify_oracle` re-drives the oracle and diffs against goldens
//! (a deterministic-by-construction sanity check that also proves the
//! capture is reproducible).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use oracle::{Oracle, OracleConfig, OracleInvocation, OracleOutput, OracleRun};

use crate::diff::{Diff, ManifestDiff, diff_bytes, diff_manifests};
use crate::fixture::{Fixture, ToolName};
use crate::manifest::{Manifest, OracleSummary, OutputEntry, RunSummary};
use crate::timefmt;

/// Per-compiler clock anchors. Each value must match the corresponding
/// `oracle::FakeTime::*().instant` so inputs materialized for our
/// toolchain see the same mtime the oracle pinned them to.
///   - bcc:  1991-04-23 12:00:00 UTC = BC2 release date
///   - msc:  1987-10-15 12:00:00 UTC = MSC 5.0 release date
const PIN_EPOCH_SECS_BCC: u64 = 672_408_000;
const PIN_EPOCH_SECS_MSC: u64 = 561_297_600;

fn pin_epoch_secs_for(compiler: &str) -> u64 {
    match compiler {
        "msc" => PIN_EPOCH_SECS_MSC,
        // Default to bcc for any unrecognized compiler so existing
        // bcc fixtures continue to anchor at the BC2 instant.
        _ => PIN_EPOCH_SECS_BCC,
    }
}

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

/// Verify by running **our toolchain** against the fixture and diffing
/// against the captured goldens. `tool_paths` resolves which host binary
/// implements each oracle tool.
///
/// `stdout`/`stderr` differences are reported but **not gating**: a stream
/// mismatch alone doesn't make `is_empty()` return false. The byte-exact
/// contract is about output files and exit codes; BCC's stdout banner
/// contains "Available memory NNNNNN" reporting DOSBox-emulated DOS RAM,
/// which a native Rust binary can't reproduce. (Use [`verify_oracle`] —
/// which is strict — to check the goldens themselves haven't drifted.)
///
/// # Errors
/// Returns [`HarnessError`] if our toolchain fails to spawn or the
/// fixture has no `expected/` to compare against.
pub fn verify_ours(
    fixture: &Fixture,
    tool_paths: &ToolPaths,
) -> Result<Diff, HarnessError> {
    let expected_manifest = load_expected_manifest(fixture)?;
    let run = run_ours(fixture, tool_paths)?;
    let actual_manifest = build_manifest(fixture, &run, our_summary(fixture));

    let manifest_diffs: Vec<_> = diff_manifests(&expected_manifest, &actual_manifest)
        .into_iter()
        .filter(|d| {
            // Stream-related sha diffs are informational under
            // verify_ours; everything else stays gating.
            !matches!(d, ManifestDiff::StdoutSha { .. } | ManifestDiff::StderrSha { .. })
        })
        .collect();
    let mut diff = Diff { manifest: manifest_diffs, ..Diff::default() };

    let expected_dir = fixture.expected_dir();
    // Streams are noted as advisory in the report but don't gate.
    for (name, expected, actual) in [
        ("stdout", read_file(&expected_dir, "stdout")?, run.stdout.clone()),
        ("stderr", read_file(&expected_dir, "stderr")?, run.stderr.clone()),
    ] {
        if let Some(d) = diff_bytes(name, &expected, &actual) {
            diff.advisory.push(d);
        }
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

/// Which host binaries implement each oracle tool. `None` means "not yet
/// implemented", and using a fixture that demands it will fail with
/// [`HarnessError::ToolNotImplemented`].
#[derive(Debug, Clone, Default)]
pub struct ToolPaths {
    pub bcc: Option<PathBuf>,
    pub tasm: Option<PathBuf>,
    pub tlink: Option<PathBuf>,
    /// Our future MSC reimplementation (`crates/msc`). `None` today —
    /// `--toolchain ours` against an MSC fixture currently has no
    /// path to spawn.
    pub msc: Option<PathBuf>,
}

impl ToolPaths {
    /// Look for `target/debug/{bcc,tasm,tlink,msc}` under the workspace root.
    /// Whichever binaries exist are bound; the rest stay `None`.
    #[must_use]
    pub fn from_workspace_debug(workspace_root: &Path) -> Self {
        let pick = |name: &str| {
            let candidate = workspace_root.join("target").join("debug").join(name);
            candidate.is_file().then_some(candidate)
        };
        Self {
            bcc: pick("bcc"),
            tasm: pick("tasm"),
            tlink: pick("tlink"),
            msc: pick("msc"),
        }
    }

    fn resolve(&self, tool: ToolName) -> Result<&Path, HarnessError> {
        let opt = match tool {
            ToolName::Bcc => &self.bcc,
            ToolName::Tasm => &self.tasm,
            ToolName::Tlink => &self.tlink,
            ToolName::Cl => &self.msc,
            // MSC ships MASM and LINK with a separate driver; we'll
            // grow these slots when `crates/msc` lands proper
            // reimplementations.
            ToolName::Masm | ToolName::Link => &None,
        };
        opt.as_deref().ok_or(HarnessError::ToolNotImplemented(tool.as_str().to_owned()))
    }
}

fn run_ours(fixture: &Fixture, tool_paths: &ToolPaths) -> Result<OracleRun, HarnessError> {
    let tool = fixture.invocation.tool;
    let bin = tool_paths.resolve(tool)?;
    let inputs = fixture.load_inputs()?;
    let work = make_workdir()?;

    // Materialize inputs with the pinned mtime — mirrors what
    // `oracle::dosbox::materialize_inputs` does so our compiler sees the
    // same source mtime as the oracle's BCC.
    let pin = UNIX_EPOCH + Duration::from_secs(pin_epoch_secs_for(&fixture.compiler));
    let mut input_names: BTreeSet<String> = BTreeSet::new();
    for (name, bytes) in &inputs {
        let path = work.join(name);
        fs::write(&path, bytes)?;
        let f = fs::File::options().write(true).open(&path)?;
        f.set_modified(pin)?;
        input_names.insert(name.clone());
    }

    let output = Command::new(bin)
        .args(&fixture.invocation.args)
        .current_dir(&work)
        .output()
        .map_err(|e| HarnessError::ToolSpawn(bin.to_owned(), e.to_string()))?;

    let stdout = output.stdout;
    let stderr = output.stderr;
    let exit_code = output.status.code().unwrap_or(-1);

    // Collect outputs the same way the oracle does: every file in the work
    // dir that wasn't an input, with its mtime normalized to the pin so the
    // manifest stays deterministic.
    let mut outputs = BTreeMap::new();
    for entry in fs::read_dir(&work)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_uppercase();
        if input_names.contains(&name) {
            continue;
        }
        let path = entry.path();
        let bytes = fs::read(&path)?;
        let f = fs::File::options().write(true).open(&path)?;
        f.set_modified(pin)?;
        let mtime = Some(pin);
        outputs.insert(name, OracleOutput { bytes, mtime });
    }

    let _ = fs::remove_dir_all(&work);
    Ok(OracleRun { exit_code, stdout, stderr, outputs })
}

fn make_workdir() -> Result<PathBuf, HarnessError> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    for n in 0u32..1024 {
        let candidate = base.join(format!("borland-c20-fixtures-{pid}-{n}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(HarnessError::Io(e)),
        }
    }
    Err(HarnessError::Io(std::io::Error::other(
        "could not allocate workdir",
    )))
}

fn our_summary(fixture: &Fixture) -> OracleSummary {
    OracleSummary {
        tool: fixture.invocation.tool.as_str().to_owned(),
        args: fixture.invocation.args.clone(),
        dosbox_version: None,
        fake_time: Some(faketime_iso_for(&fixture.compiler).to_owned()),
    }
}

fn run_oracle(workspace_root: &Path, fixture: &Fixture) -> Result<OracleRun, HarnessError> {
    let cfg = oracle_config_for(workspace_root, &fixture.compiler)?;
    let inputs = fixture.load_inputs()?;
    let oracle = Oracle::open(cfg).map_err(|e| HarnessError::Oracle(e.to_string()))?;
    let mut inv = OracleInvocation::new(fixture.invocation.tool.as_oracle())
        .args(fixture.invocation.args.clone());
    for (name, bytes) in &inputs {
        inv = inv.input(name.clone(), bytes.as_slice());
    }
    oracle.run(&inv).map_err(|e| HarnessError::Oracle(e.to_string()))
}

/// Map a fixture's `compiler` field to the right [`OracleConfig`].
/// Today we recognize "bcc" (BC2.zip) and "msc" (MSC500.zip); future
/// vendors are a one-line addition. Anything else is a harness-time
/// error rather than a silent fall-through to BC2.
fn oracle_config_for(
    workspace_root: &Path,
    compiler: &str,
) -> Result<OracleConfig, HarnessError> {
    match compiler {
        "bcc" => Ok(OracleConfig::for_workspace(workspace_root)),
        "msc" => Ok(OracleConfig::for_msc500_workspace(workspace_root)),
        other => Err(HarnessError::Oracle(format!(
            "no oracle profile registered for compiler {other:?}"
        ))),
    }
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
        fake_time: Some(faketime_iso_for(&fixture.compiler).to_owned()),
    }
}

fn faketime_iso_for(compiler: &str) -> &'static str {
    match compiler {
        // BC2 release date.
        "bcc" => "1991-04-23T12:00:00Z",
        // MSC 5.0 release date.
        "msc" => "1987-10-15T12:00:00Z",
        // Fall back to BC2 — pre-existing manifests for `bcc` fixtures
        // expect this exact string and we don't want a future
        // unrecognized compiler to silently change captured goldens.
        _ => "1991-04-23T12:00:00Z",
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
    #[error("tool not yet implemented in our toolchain: {0}")]
    ToolNotImplemented(String),
    #[error("could not run tool {0}: {1}")]
    ToolSpawn(PathBuf, String),
}

