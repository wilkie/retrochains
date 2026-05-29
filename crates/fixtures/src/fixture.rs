//! Loading a fixture from disk. A fixture is a directory containing:
//! - `invocation.<compiler>.toml` (the recipe, per compiler)
//! - one or more input files (typically DOS-style 8.3 uppercased)
//! - `expected/<compiler>/` (the goldens for each compiler, populated by `capture`)
//!
//! The compiler name (`bcc`, `msc`, ...) is supplied by the caller —
//! typically threaded down from the `xfix --compiler` flag. Today only
//! `bcc` is implemented; the per-compiler fan-out exists so a second
//! reimplementation target lands without touching every fixture. See
//! `specs/plans/SECOND_COMPILER.md`.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use oracle::Tool;

/// The tool to invoke. Mirrors `oracle::Tool` but is serde-friendly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolName {
    Bcc,
    Tasm,
    Tlink,
}

impl ToolName {
    #[must_use]
    pub fn as_oracle(self) -> Tool {
        match self {
            Self::Bcc => Tool::Bcc,
            Self::Tasm => Tool::Tasm,
            Self::Tlink => Tool::Tlink,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bcc => "bcc",
            Self::Tasm => "tasm",
            Self::Tlink => "tlink",
        }
    }
}

/// The contents of `invocation.<compiler>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation {
    /// Which oracle tool to run.
    pub tool: ToolName,
    /// Arguments passed verbatim. DOS filenames here must match input files
    /// in the fixture directory (case-insensitive on DOS; we materialize
    /// them uppercased in the work directory).
    pub args: Vec<String>,
    /// Which files at the fixture root to materialize into the DOS work
    /// directory. Defaults to every file at the fixture root that isn't
    /// an `invocation.*.toml` or the `expected/` directory.
    #[serde(default)]
    pub inputs: Option<Vec<String>>,
    /// Free-form description kept in git for context. Not checked by
    /// verify; documentation only.
    #[serde(default)]
    pub description: Option<String>,
}

/// A loaded fixture, scoped to a single compiler.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub root: PathBuf,
    pub name: String,
    pub compiler: String,
    pub invocation: Invocation,
}

impl Fixture {
    /// Read `<root>/invocation.<compiler>.toml` and return the parsed
    /// fixture scoped to that compiler. Fails with `Layout` if the
    /// fixture has no invocation file for the requested compiler — a
    /// caller doing a corpus sweep should treat that as "skip" rather
    /// than "error".
    ///
    /// # Errors
    /// Returns [`LoadError::Io`] for filesystem errors, [`LoadError::Toml`]
    /// if the invocation file doesn't parse, and [`LoadError::Layout`]
    /// if no `invocation.<compiler>.toml` is present.
    pub fn load(root: impl Into<PathBuf>, compiler: &str) -> Result<Self, LoadError> {
        let root = root.into();
        let inv_name = format!("invocation.{compiler}.toml");
        let inv_path = root.join(&inv_name);
        if !inv_path.is_file() {
            return Err(LoadError::Layout(format!(
                "no {inv_name} at {}",
                inv_path.display()
            )));
        }
        let inv_text = fs::read_to_string(&inv_path)?;
        let invocation: Invocation = toml::from_str(&inv_text)?;
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unnamed>")
            .to_owned();
        Ok(Self { root, name, compiler: compiler.to_owned(), invocation })
    }

    /// Path to the `expected/<compiler>/` directory (which may not
    /// yet exist).
    #[must_use]
    pub fn expected_dir(&self) -> PathBuf {
        self.root.join("expected").join(&self.compiler)
    }

    /// Resolve the list of input files to materialize, applying the default
    /// (every file at the fixture root that isn't an `invocation.*.toml`
    /// or the `expected/` directory).
    ///
    /// # Errors
    /// Returns [`LoadError::Io`] if the directory can't be listed.
    pub fn resolved_inputs(&self) -> Result<Vec<String>, LoadError> {
        if let Some(explicit) = &self.invocation.inputs {
            return Ok(explicit.clone());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip every `invocation.<compiler>.toml` — the per-
            // compiler recipes are not fixture inputs.
            if name.starts_with("invocation.") && name.ends_with(".toml") {
                continue;
            }
            names.push(name);
        }
        names.sort();
        Ok(names)
    }

    /// Load each input file as `(dos_name, bytes)` ready to pass to the
    /// oracle. `dos_name` is the input's basename uppercased.
    ///
    /// # Errors
    /// Returns [`LoadError::Io`] if any input file can't be read, or
    /// [`LoadError::Layout`] if an input listed in the invocation file
    /// is missing from disk.
    pub fn load_inputs(&self) -> Result<Vec<(String, Vec<u8>)>, LoadError> {
        let names = self.resolved_inputs()?;
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            let path = self.root.join(&n);
            if !path.is_file() {
                return Err(LoadError::Layout(format!(
                    "input listed in invocation.{}.toml missing on disk: {}",
                    self.compiler,
                    path.display()
                )));
            }
            out.push((n.to_uppercase(), fs::read(&path)?));
        }
        Ok(out)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("fixture layout: {0}")]
    Layout(String),
}
