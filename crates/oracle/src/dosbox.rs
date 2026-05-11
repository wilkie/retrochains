//! Drive DOSBox so it runs one of the Borland tools and returns whatever it
//! produced. Everything happens in a fresh per-invocation working directory
//! mounted as DOS drive `D:`; the extracted BC2 tree is mounted as `C:`.
//!
//! The actual tool invocation lives in a `_RUN.BAT` script written into the
//! work directory before we launch DOSBox. We have to use a batch file (rather
//! than chain `-c` commands on the DOSBox command line) because DOSBox 0.74's
//! shell applies `>` redirects unconditionally even when the `IF ERRORLEVEL`
//! guarding them is false — which truncates any sentinel files we try to
//! write. A batch file with `GOTO`/labels means only one branch's redirect
//! ever runs, sidestepping the bug.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::bc2::Bc2Layout;
use crate::{FakeTime, OracleRun, Tool};

/// Files written by our batch wrapper (not by the tool itself). They are
/// filtered out of the returned `outputs` map.
const STDOUT_FILE: &str = "_OUT.TXT";
const EXITCODE_FILE: &str = "_RC.TXT";
const RUN_BAT: &str = "_RUN.BAT";

#[derive(Debug, thiserror::Error)]
pub enum DosboxError {
    #[error("dosbox not runnable ({0}): {1}")]
    Spawn(PathBuf, io::Error),
    #[error("dosbox exited with status {0}")]
    NonZero(i32),
    #[error("dosbox produced no exit-code sentinel ({EXITCODE_FILE} missing)")]
    NoExitCode,
    #[error("could not parse exit-code sentinel: {0:?}")]
    BadExitCode(Vec<u8>),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

pub(crate) fn run(
    dosbox: &Path,
    fake_time: Option<&FakeTime>,
    layout: &Bc2Layout,
    tool: Tool,
    args: &[String],
    inputs: &BTreeMap<String, &[u8]>,
) -> Result<OracleRun, DosboxError> {
    let work = TempWorkDir::new()?;
    materialize_inputs(work.path(), inputs, fake_time.map(|ft| ft.instant))?;
    write_run_bat(work.path(), tool, args)?;

    let bc2_dir = layout.bc2_dir.canonicalize()?;
    let work_dir = work.path().canonicalize()?;

    let mut cmd = build_command(dosbox, fake_time);
    // TZ=UTC so the FAKETIME timestamp (which has no timezone marker) is
    // interpreted consistently regardless of the host's local timezone.
    cmd.env("TZ", "UTC")
        .env("SDL_VIDEODRIVER", "dummy")
        .arg("-exit")
        .arg("-c").arg(format!("mount c \"{}\"", bc2_dir.display()))
        .arg("-c").arg(format!("mount d \"{}\"", work_dir.display()))
        .arg("-c").arg("set INCLUDE=C:\\INCLUDE")
        .arg("-c").arg("set LIB=C:\\LIB")
        .arg("-c").arg("PATH C:\\BIN")
        .arg("-c").arg("d:")
        .arg("-c").arg(RUN_BAT);

    let status = cmd
        .status()
        .map_err(|e| spawn_error(dosbox, fake_time, e))?;
    if !status.success() {
        return Err(DosboxError::NonZero(status.code().unwrap_or(-1)));
    }

    let exit_code = read_exit_code(work.path())?;
    let stdout = fs::read(work.path().join(STDOUT_FILE)).unwrap_or_default();
    let outputs = collect_outputs(work.path(), inputs)?;

    Ok(OracleRun { exit_code, stdout, stderr: Vec::new(), outputs })
}

/// Build the base `Command` — either `dosbox` directly, or
/// `faketime -f "@<ts>" dosbox` when clock-pinning is requested.
///
/// The `-f` flag bypasses faketime's `date(1)`-based timestamp validation
/// (which rejects the `@<date-string>` form we need). The leading `@` then
/// tells libfaketime to *freeze* time at that instant rather than let it
/// advance — important because DOSBox makes many time calls per run and we
/// need every one of them to return the same value.
fn build_command(dosbox: &Path, fake_time: Option<&FakeTime>) -> Command {
    if let Some(ft) = fake_time {
        let mut cmd = Command::new(&ft.binary);
        cmd.arg("-f").arg(format!("@{}", ft.timestamp));
        cmd.arg(dosbox);
        cmd
    } else {
        Command::new(dosbox)
    }
}

fn spawn_error(dosbox: &Path, fake_time: Option<&FakeTime>, err: io::Error) -> DosboxError {
    // When the wrapper is in play and the spawn failed, the missing binary is
    // far more likely to be `faketime` than `dosbox`, so surface that path.
    let path = fake_time.map_or_else(|| dosbox.to_path_buf(), |ft| ft.binary.clone());
    DosboxError::Spawn(path, err)
}

fn write_run_bat(work: &Path, tool: Tool, args: &[String]) -> io::Result<()> {
    // CRLF line endings; the DOS shell handles either but CRLF avoids surprises
    // (and matches what a real DOS-built .BAT looks like, which is one less
    // thing to wonder about if a future bug points at line endings).
    let mut bat = String::new();
    bat.push_str("@echo off\r\n");
    bat.push_str(tool.dos_name());
    for arg in args {
        bat.push(' ');
        bat.push_str(arg);
    }
    bat.push_str(" > ");
    bat.push_str(STDOUT_FILE);
    bat.push_str("\r\n");
    bat.push_str("IF ERRORLEVEL 1 GOTO FAIL\r\n");
    bat.push_str("ECHO 0 > ");
    bat.push_str(EXITCODE_FILE);
    bat.push_str("\r\n");
    bat.push_str("GOTO DONE\r\n");
    bat.push_str(":FAIL\r\n");
    bat.push_str("ECHO 1 > ");
    bat.push_str(EXITCODE_FILE);
    bat.push_str("\r\n");
    bat.push_str(":DONE\r\n");
    // EXIT inside the batch is required. DOSBox 0.74 doesn't fire subsequent
    // `-c` commands after a batch invocation returns, so a trailing
    // `-c "exit"` on the dosbox command line is silently dropped.
    bat.push_str("EXIT\r\n");
    fs::write(work.join(RUN_BAT), bat)
}

fn materialize_inputs(
    work: &Path,
    inputs: &BTreeMap<String, &[u8]>,
    pin_mtime: Option<SystemTime>,
) -> io::Result<()> {
    for (name, bytes) in inputs {
        let path = work.join(name);
        fs::write(&path, bytes)?;
        // BCC reads the source's DOS-packed mtime via INT 21h AH=57h and
        // embeds it directly into the OMF. Pin it so byte-exact output
        // doesn't depend on when the test happened to run.
        if let Some(mtime) = pin_mtime {
            let f = fs::File::options().write(true).open(&path)?;
            f.set_modified(mtime)?;
        }
    }
    Ok(())
}

fn read_exit_code(work: &Path) -> Result<i32, DosboxError> {
    let path = work.join(EXITCODE_FILE);
    if !path.is_file() {
        return Err(DosboxError::NoExitCode);
    }
    let raw = fs::read(&path)?;
    let trimmed: Vec<u8> =
        raw.iter().copied().take_while(|b| !b.is_ascii_whitespace()).collect();
    let text = std::str::from_utf8(&trimmed).map_err(|_| DosboxError::BadExitCode(raw.clone()))?;
    text.parse().map_err(|_| DosboxError::BadExitCode(raw))
}

fn collect_outputs(
    work: &Path,
    inputs: &BTreeMap<String, &[u8]>,
) -> io::Result<BTreeMap<String, Vec<u8>>> {
    let mut outputs = BTreeMap::new();
    for entry in fs::read_dir(work)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_uppercase();
        let is_orchestration =
            name == STDOUT_FILE || name == EXITCODE_FILE || name == RUN_BAT;
        if !is_orchestration && !inputs.contains_key(&name) {
            outputs.insert(name, fs::read(entry.path())?);
        }
    }
    Ok(outputs)
}

/// A temp working directory that cleans itself up. We avoid the `tempfile`
/// crate to keep the dependency list short; this is the only place we need it.
struct TempWorkDir {
    path: PathBuf,
}

impl TempWorkDir {
    fn new() -> io::Result<Self> {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        for n in 0u32..1024 {
            let candidate = base.join(format!("borland-c20-oracle-{pid}-{n}"));
            match fs::create_dir(&candidate) {
                Ok(()) => return Ok(Self { path: candidate }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("could not allocate a temp working directory"))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        if std::env::var_os("ORACLE_KEEP_WORKDIR").is_some() {
            eprintln!("[oracle] keeping workdir: {}", self.path.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}
