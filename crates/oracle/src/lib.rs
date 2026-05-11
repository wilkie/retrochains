//! Oracle: runs the original Borland C++ 2.0 toolchain (`BCC.EXE`, `TASM.EXE`,
//! `TLINK.EXE`) under DOSBox so the rest of this workspace can diff its output
//! byte-for-byte against the reference compiler.
//!
//! The Borland install tree lives in `BC2.zip` at the repository root. On first
//! use the oracle extracts it (lazily, idempotently) to a gitignored `.bc2/`
//! directory and reuses that extraction afterwards.

mod bc2;
mod dosbox;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use bc2::Bc2Layout;
pub use dosbox::DosboxError;

/// One of the original Borland tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Bcc,
    Tasm,
    Tlink,
}

impl Tool {
    /// Name of the DOS executable (no extension; DOS will resolve `.EXE`).
    #[must_use]
    pub fn dos_name(self) -> &'static str {
        match self {
            Self::Bcc => "BCC",
            Self::Tasm => "TASM",
            Self::Tlink => "TLINK",
        }
    }
}

/// One oracle invocation: inputs to drop into the DOS working directory, plus
/// the arguments to pass to the tool.
#[derive(Debug, Default)]
pub struct OracleInvocation<'a> {
    pub tool: Option<Tool>,
    pub args: Vec<String>,
    /// Files to materialize in the DOS working directory before running. Keyed
    /// by the DOS-visible filename (e.g. `"FOO.CPP"`).
    pub inputs: BTreeMap<String, &'a [u8]>,
}

impl<'a> OracleInvocation<'a> {
    #[must_use]
    pub fn new(tool: Tool) -> Self {
        Self { tool: Some(tool), args: Vec::new(), inputs: BTreeMap::new() }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn input(mut self, name: impl Into<String>, bytes: &'a [u8]) -> Self {
        self.inputs.insert(name.into(), bytes);
        self
    }
}

/// A single file produced by an oracle run.
#[derive(Debug, Clone)]
pub struct OracleOutput {
    /// Raw bytes of the file as the DOS tool wrote it.
    pub bytes: Vec<u8>,
    /// Modification time of the file at the moment the tool exited, as
    /// reported by the host filesystem. DOS tools propagate / set mtimes
    /// (e.g. BCC stamps its output OBJ with the source's mtime), so this
    /// is part of the byte-exact contract.
    pub mtime: Option<SystemTime>,
}

/// What a single oracle run produced.
#[derive(Debug)]
pub struct OracleRun {
    /// Exit code reported by the DOS tool (captured via `errorlevel`).
    pub exit_code: i32,
    /// Captured stdout from the tool.
    pub stdout: Vec<u8>,
    /// Captured stderr. May be empty if DOSBox 0.74's shell didn't honor
    /// the `2>` redirect for this invocation — Borland tools also tend to
    /// write diagnostics to stdout anyway.
    pub stderr: Vec<u8>,
    /// Every file the tool produced or modified in the working directory at
    /// exit time, keyed by DOS filename (uppercase). Caller-provided input
    /// files are filtered out.
    pub outputs: BTreeMap<String, OracleOutput>,
}

#[derive(Debug, thiserror::Error)]
pub enum OracleError {
    #[error("BC2.zip not found at {0}")]
    Bc2ZipMissing(PathBuf),
    #[error("could not unpack BC2.zip: {0}")]
    Unpack(#[from] zip::result::ZipError),
    #[error("oracle invocation requires a tool")]
    MissingTool,
    #[error("DOSBox: {0}")]
    Dosbox(#[from] DosboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Pin every observable clock reading to a single fixed instant. The oracle
/// applies this two ways: (a) DOSBox is launched under `faketime` so calls
/// like `gettimeofday`/`time` see the pinned instant, and (b) any input file
/// we materialize in the DOS work directory has its mtime stamped to the
/// same instant. The latter matters because BCC reads the source file's
/// mtime via DOS stat (`INT 21h` AH=57h), which doesn't go through the libc
/// time functions faketime intercepts — without normalizing mtimes, our
/// inputs would drift the OMF timestamp on every run anyway.
#[derive(Debug, Clone)]
pub struct FakeTime {
    /// Path to the `faketime` command (defaults to looking it up in PATH).
    pub binary: PathBuf,
    /// The instant as a string `faketime` accepts (`YYYY-MM-DD hh:mm:ss`).
    /// Interpreted in UTC because we set `TZ=UTC` when spawning DOSBox.
    pub timestamp: String,
    /// The same instant as a `SystemTime`, used for input-file mtimes.
    /// Must denote the same moment as `timestamp` — the [`Default`] impl
    /// keeps them in sync; if you customize one, customize both.
    pub instant: SystemTime,
}

impl Default for FakeTime {
    fn default() -> Self {
        // BC2's own release date, interpreted as UTC. The numeric constant
        // matches the timestamp string (`date -u -d "1991-04-23 12:00:00 UTC"
        // +%s` == 672408000).
        Self {
            binary: PathBuf::from("faketime"),
            timestamp: "1991-04-23 12:00:00".to_owned(),
            // 1991-04-23 12:00:00 UTC as Unix epoch seconds. Clippy would
            // rather we wrote `from_hours(186_780)`, but the constructor is
            // unstable and "epoch seconds" reads more obviously as a date.
            #[allow(clippy::duration_suboptimal_units)]
            instant: SystemTime::UNIX_EPOCH + Duration::from_secs(672_408_000),
        }
    }
}

/// Configuration for the oracle. Defaults are correct for in-repo use.
#[derive(Debug, Clone)]
pub struct OracleConfig {
    /// Where the BC2.zip archive lives.
    pub bc2_zip: PathBuf,
    /// Where to extract it on first use (and reuse thereafter).
    pub bc2_root: PathBuf,
    /// Path to the dosbox binary.
    pub dosbox: PathBuf,
    /// If set, DOSBox runs under `faketime` so the emulated DOS clock is
    /// pinned and BCC's timestamp-embedding produces reproducible output.
    /// Default is `Some(FakeTime::default())` because byte-exact
    /// reproducibility is the whole point of the oracle. Set to `None` only
    /// when you specifically want the real clock (you almost never do).
    pub fake_time: Option<FakeTime>,
}

impl OracleConfig {
    /// Defaults based on the workspace root containing `BC2.zip`.
    #[must_use]
    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self {
            bc2_zip: workspace_root.join("BC2.zip"),
            bc2_root: workspace_root.join(".bc2"),
            dosbox: PathBuf::from("dosbox"),
            fake_time: Some(FakeTime::default()),
        }
    }
}

/// The oracle. Owns the lazily-extracted BC2 install tree and drives DOSBox.
#[derive(Debug)]
pub struct Oracle {
    cfg: OracleConfig,
    layout: Bc2Layout,
}

impl Oracle {
    /// Ensure BC2.zip is extracted and ready, then return a handle.
    ///
    /// # Errors
    /// Returns [`OracleError::Bc2ZipMissing`] if the archive isn't where the
    /// config says it is, [`OracleError::Unpack`] if extraction fails, or
    /// [`OracleError::Io`] for unrelated filesystem issues.
    pub fn open(cfg: OracleConfig) -> Result<Self, OracleError> {
        let layout = bc2::ensure_extracted(&cfg.bc2_zip, &cfg.bc2_root)?;
        Ok(Self { cfg, layout })
    }

    /// Path to the extracted BC2 root (the directory that contains BIN/,
    /// INCLUDE/, LIB/).
    #[must_use]
    pub fn layout(&self) -> &Bc2Layout {
        &self.layout
    }

    /// Run one oracle invocation.
    ///
    /// # Errors
    /// Returns [`OracleError::MissingTool`] if the invocation didn't pick a
    /// tool, or [`OracleError::Dosbox`] for any failure in the emulator
    /// invocation (spawn failure, missing exit-code sentinel, etc.).
    pub fn run(&self, invocation: &OracleInvocation<'_>) -> Result<OracleRun, OracleError> {
        let tool = invocation.tool.ok_or(OracleError::MissingTool)?;
        dosbox::run(
            &self.cfg.dosbox,
            self.cfg.fake_time.as_ref(),
            &self.layout,
            tool,
            &invocation.args,
            &invocation.inputs,
        )
        .map_err(Into::into)
    }
}
