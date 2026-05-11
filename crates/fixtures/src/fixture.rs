//! Loading a fixture from disk. A fixture is a directory containing:
//! - `invocation.toml` (the recipe)
//! - one or more input files (typically DOS-style 8.3 uppercased)
//! - `expected/` (the goldens, populated by `capture`)

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

/// The contents of `invocation.toml`.
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
    /// `invocation.toml` or `expected/`.
    #[serde(default)]
    pub inputs: Option<Vec<String>>,
    /// Free-form description kept in git for context. Not checked by
    /// verify; documentation only.
    #[serde(default)]
    pub description: Option<String>,
}

/// A loaded fixture.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub root: PathBuf,
    pub name: String,
    pub invocation: Invocation,
}

impl Fixture {
    /// Read `<root>/invocation.toml` and return the parsed fixture.
    ///
    /// # Errors
    /// Returns [`LoadError::Io`] for filesystem errors, [`LoadError::Toml`]
    /// if `invocation.toml` doesn't parse, and [`LoadError::Layout`] if the
    /// directory doesn't look like a fixture (e.g. invocation.toml absent).
    pub fn load(root: impl Into<PathBuf>) -> Result<Self, LoadError> {
        let root = root.into();
        let inv_path = root.join("invocation.toml");
        if !inv_path.is_file() {
            return Err(LoadError::Layout(format!(
                "no invocation.toml at {}",
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
        Ok(Self { root, name, invocation })
    }

    /// Path to the `expected/` directory (which may not yet exist).
    #[must_use]
    pub fn expected_dir(&self) -> PathBuf {
        self.root.join("expected")
    }

    /// Resolve the list of input files to materialize, applying the default
    /// (every file at the fixture root that isn't reserved).
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
            if name == "invocation.toml" {
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
    /// [`LoadError::Layout`] if an input listed in `invocation.toml` is
    /// missing from disk.
    pub fn load_inputs(&self) -> Result<Vec<(String, Vec<u8>)>, LoadError> {
        let names = self.resolved_inputs()?;
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            let path = self.root.join(&n);
            if !path.is_file() {
                return Err(LoadError::Layout(format!(
                    "input listed in invocation.toml missing on disk: {}",
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
