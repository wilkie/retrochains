//! Oracle: runs an original 16-bit DOS toolchain under DOSBox so the
//! rest of this workspace can diff its output byte-for-byte against
//! the reference compiler.
//!
//! Supported distributions: Borland C++ 2.0 (`BC2.zip`, lazy-extracted
//! to `.bc2/`) and Microsoft C 5.0 (`MSC500.zip`, lazy-extracted to
//! `.msc500/`). Pick one via [`OracleConfig::for_workspace`] (BC2 by
//! default) or [`OracleConfig::for_msc500_workspace`].

mod distro;
mod dosbox;
pub mod provision;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use distro::{DistroLayout, DistroSpec};
pub use dosbox::DosboxError;
pub use provision::{InstallRecipe, ProvisionError, ProvisionSpec, VerifyReport};

/// One of the original DOS tools we can drive. Each oracle distribution
/// supports a subset — BC2 ships BCC/TASM/TLINK; MSC500 ships CL/MASM/LINK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Bcc,
    Tasm,
    Tlink,
    /// Microsoft C 5.0 driver. Shells out to C1/C2/C3 internally,
    /// then to MASM and LINK as needed.
    Cl,
    /// MASM 5.x (ships in MSC500.zip's BIN/).
    Masm,
    /// MSC's LINK.EXE — distinct from BCC's TLINK.EXE.
    Link,
}

impl Tool {
    /// Name of the DOS executable (no extension; DOS will resolve `.EXE`).
    #[must_use]
    pub fn dos_name(self) -> &'static str {
        match self {
            Self::Bcc => "BCC",
            Self::Tasm => "TASM",
            Self::Tlink => "TLINK",
            Self::Cl => "CL",
            Self::Masm => "MASM",
            Self::Link => "LINK",
        }
    }
}

/// One oracle invocation: inputs to drop into the DOS working directory, plus
/// the arguments to pass to the tool.
#[derive(Debug, Default)]
pub struct OracleInvocation<'a> {
    pub tool: Option<Tool>,
    pub args: Vec<String>,
    /// Optional second-pass args set, chained into the same DOSBox
    /// session after `args`. Used by fixtures that need both an
    /// OBJ and an ASM listing (BCC's `-c`/`-S` are mutually exclusive,
    /// so the only way to capture both is two compile commands).
    pub asm_args: Option<Vec<String>>,
    /// Files to materialize in the DOS working directory before running. Keyed
    /// by the DOS-visible filename (e.g. `"FOO.CPP"`).
    pub inputs: BTreeMap<String, &'a [u8]>,
}

impl<'a> OracleInvocation<'a> {
    #[must_use]
    pub fn new(tool: Tool) -> Self {
        Self { tool: Some(tool), args: Vec::new(), asm_args: None, inputs: BTreeMap::new() }
    }

    #[must_use]
    pub fn with_asm_args(mut self, args: Vec<String>) -> Self {
        self.asm_args = Some(args);
        self
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
    #[error("oracle archive not found at {0}")]
    ArchiveMissing(PathBuf),
    #[error("could not unpack oracle archive: {0}")]
    Unpack(#[from] zip::result::ZipError),
    #[error("oracle invocation requires a tool")]
    MissingTool,
    #[error("tool {tool:?} is not part of the {distro:?} distribution")]
    ToolNotInDistribution { tool: Tool, distro: &'static str },
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
        Self::bc2()
    }
}

impl FakeTime {
    /// Anchor for the BC2 oracle — Borland C++ 2.0's release date,
    /// interpreted as UTC.
    #[must_use]
    pub fn bc2() -> Self {
        Self {
            binary: PathBuf::from("faketime"),
            timestamp: "1991-04-23 12:00:00".to_owned(),
            // 1991-04-23 12:00:00 UTC as Unix epoch seconds.
            #[allow(clippy::duration_suboptimal_units)]
            instant: SystemTime::UNIX_EPOCH + Duration::from_secs(672_408_000),
        }
    }

    /// Anchor for the MSC500 oracle — Microsoft C 5.0's release date
    /// (1987-10-15 05:00 — the timestamp every binary in the
    /// distribution carries). We use noon UTC of the same day so the
    /// pin is unambiguous regardless of host timezone.
    #[must_use]
    pub fn msc500() -> Self {
        Self {
            binary: PathBuf::from("faketime"),
            timestamp: "1987-10-15 12:00:00".to_owned(),
            // 1987-10-15 12:00:00 UTC as Unix epoch seconds.
            #[allow(clippy::duration_suboptimal_units)]
            instant: SystemTime::UNIX_EPOCH + Duration::from_secs(561_297_600),
        }
    }
}

/// Configuration for the oracle. Pick a distribution-aware constructor
/// (`for_workspace` for BC2, `for_msc500_workspace` for Microsoft C 5.0).
#[derive(Debug, Clone)]
pub struct OracleConfig {
    /// Which distribution this oracle drives (BC2 / MSC500 / ...).
    pub distro: DistroSpec,
    /// Path to the dosbox binary.
    pub dosbox: PathBuf,
    /// If set, DOSBox runs under `faketime` so the emulated DOS clock is
    /// pinned and the compiler's timestamp-embedding produces reproducible
    /// output. Per-distribution: BC2 uses its own release date,
    /// MSC500 uses MSC 5.0's. Set to `None` only when you specifically
    /// want the real clock (you almost never do).
    pub fake_time: Option<FakeTime>,
}

impl OracleConfig {
    /// Defaults for the Borland C++ 2.0 oracle (BC2.zip at the workspace
    /// root). The historical default — kept under this name so existing
    /// callers don't break.
    #[must_use]
    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self {
            distro: DistroSpec::bc2(workspace_root),
            dosbox: PathBuf::from("dosbox"),
            fake_time: Some(FakeTime::bc2()),
        }
    }

    /// Defaults for the Microsoft C 5.0 oracle (MSC500.zip at the
    /// workspace root). See `oracles/msc/MSC500.md` for the manifest and acquisition
    /// path; the zip itself is gitignored.
    #[must_use]
    pub fn for_msc500_workspace(workspace_root: &Path) -> Self {
        Self {
            distro: DistroSpec::msc500(workspace_root),
            dosbox: PathBuf::from("dosbox"),
            fake_time: Some(FakeTime::msc500()),
        }
    }
}

/// A supported compiler toolchain: its `--compiler` name and the
/// constructors for everything that varies per vendor. This is the single
/// registry the fixtures harness consults, so the per-compiler facts
/// (oracle distribution, dosbox config, clock anchor) live in exactly one
/// place and can't drift out of sync across call sites.
#[derive(Debug, Clone, Copy)]
pub struct ToolchainProfile {
    /// Identifier shared by the `--compiler` flag, `invocation.<name>.toml`
    /// and `expected/<name>/`.
    pub name: &'static str,
    /// Builds the oracle config (distribution + dosbox + faketime) for a
    /// given workspace root.
    pub oracle_config: fn(&Path) -> OracleConfig,
    /// The clock anchor for this toolchain. Also embedded in `oracle_config`;
    /// exposed separately so callers that only need the time (e.g. stamping
    /// input mtimes) don't have to build a full config or supply a root.
    pub fake_time: fn() -> FakeTime,
}

/// Every compiler toolchain the project can drive.
///
/// **To add a vendor, add one row here** — together with its
/// [`DistroSpec`], [`FakeTime`], and `OracleConfig::for_*` constructor. The
/// fixtures harness and CLI discover supported compilers purely from this
/// table; nothing else needs editing.
pub const TOOLCHAINS: &[ToolchainProfile] = &[
    ToolchainProfile {
        name: "bcc",
        oracle_config: OracleConfig::for_workspace,
        fake_time: FakeTime::bc2,
    },
    ToolchainProfile {
        name: "msc",
        oracle_config: OracleConfig::for_msc500_workspace,
        fake_time: FakeTime::msc500,
    },
];

/// Look up a toolchain profile by `--compiler` name. `None` for an
/// unregistered vendor.
#[must_use]
pub fn toolchain(name: &str) -> Option<&'static ToolchainProfile> {
    TOOLCHAINS.iter().find(|t| t.name == name)
}

/// Comma-separated list of supported `--compiler` names, for error messages.
#[must_use]
pub fn supported_toolchains() -> String {
    TOOLCHAINS
        .iter()
        .map(|t| t.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The oracle. Owns the lazily-extracted distribution tree and drives
/// DOSBox. Each `Oracle` instance is scoped to one distribution; to
/// drive both BC2 and MSC500 in the same process, open two oracles.
#[derive(Debug)]
pub struct Oracle {
    cfg: OracleConfig,
    layout: DistroLayout,
}

impl Oracle {
    /// Ensure the distribution archive is extracted, then return a
    /// handle ready to drive DOSBox.
    ///
    /// # Errors
    /// Returns [`OracleError::ArchiveMissing`] if the archive isn't
    /// where the config says it is, [`OracleError::Unpack`] if
    /// extraction fails, or [`OracleError::Io`] for unrelated
    /// filesystem issues.
    pub fn open(cfg: OracleConfig) -> Result<Self, OracleError> {
        let layout = distro::ensure_extracted(&cfg.distro)?;
        Ok(Self { cfg, layout })
    }

    /// Resolved on-disk paths for the extracted distribution.
    #[must_use]
    pub fn layout(&self) -> &DistroLayout {
        &self.layout
    }

    /// Which distribution this oracle drives.
    #[must_use]
    pub fn distro(&self) -> &DistroSpec {
        &self.cfg.distro
    }

    /// Run one oracle invocation.
    ///
    /// # Errors
    /// Returns [`OracleError::MissingTool`] if the invocation didn't pick a
    /// tool, or [`OracleError::Dosbox`] for any failure in the emulator
    /// invocation (spawn failure, missing exit-code sentinel, etc.).
    pub fn run(&self, invocation: &OracleInvocation<'_>) -> Result<OracleRun, OracleError> {
        let tool = invocation.tool.ok_or(OracleError::MissingTool)?;
        if let Some(asm_args) = &invocation.asm_args {
            let arg_sets = vec![invocation.args.clone(), asm_args.clone()];
            dosbox::run_chained(
                &self.cfg.dosbox,
                self.cfg.fake_time.as_ref(),
                &self.layout,
                tool,
                &arg_sets,
                &invocation.inputs,
            )
            .map_err(Into::into)
        } else {
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
}
