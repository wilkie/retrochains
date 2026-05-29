//! Lazy extraction of an oracle distribution (BC2 / MSC500 / …) into a
//! gitignored cache directory. A `DistroSpec` captures everything that
//! varies between distributions (archive path, sentinel file, top-level
//! layout); `ensure_extracted` is otherwise identical across vendors.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::OracleError;

/// Where the extracted toolchain lives and how to address it inside
/// DOSBox. Built by `ensure_extracted` once the archive is unpacked.
#[derive(Debug, Clone)]
pub struct DistroLayout {
    /// Host-side directory that DOSBox will mount as DOS drive `C:`.
    /// Always contains `BIN/`, `INCLUDE/`, and `LIB/` at its top level.
    pub root_dir: PathBuf,
    /// `<root_dir>/BIN` — contains the driver binary and helper tools.
    pub bin: PathBuf,
    /// `<root_dir>/INCLUDE` — system headers.
    pub include: PathBuf,
    /// `<root_dir>/LIB` — startup objects and runtime libraries.
    pub lib: PathBuf,
}

/// Per-distribution configuration. Plug a new vendor in by adding a
/// constructor here and a matching `OracleConfig::for_*_workspace`.
#[derive(Debug, Clone)]
pub struct DistroSpec {
    /// Identifier — matches the `--compiler` flag in xfix and the
    /// `invocation.<name>.toml` filename.
    pub name: &'static str,
    /// Where the archive lives on disk (e.g. `BC2.zip`, `MSC500.zip`).
    pub archive_path: PathBuf,
    /// Where to lazy-extract on first use (e.g. `.bc2/`, `.msc500/`).
    /// Per-vendor so distributions don't collide.
    pub extract_root: PathBuf,
    /// Subdirectory inside `extract_root` that holds BIN/INCLUDE/LIB
    /// at its top level. BC2's zip wraps its tree in `BC2/`; MSC500's
    /// zip puts BIN/ at the top, so this is `""` for MSC500.
    pub root_subdir: PathBuf,
    /// File whose existence under `root_dir` (= `extract_root/root_subdir`)
    /// means a prior extraction completed. Picked to be the latest-written
    /// large binary so a partial extract is detected.
    pub sentinel: PathBuf,
}

impl DistroSpec {
    /// Borland C++ 2.0 — the reference oracle.
    #[must_use]
    pub fn bc2(workspace_root: &Path) -> Self {
        Self {
            name: "bcc",
            archive_path: workspace_root.join("BC2.zip"),
            extract_root: workspace_root.join(".bc2"),
            root_subdir: PathBuf::from("BC2"),
            sentinel: PathBuf::from("BIN/BCC.EXE"),
        }
    }

    /// Microsoft C 5.0 — the second oracle. Distribution layout is
    /// flat (BIN/INCLUDE/LIB at the top of the zip), so `root_subdir`
    /// is empty. See `MSC500.md` for acquisition and verification.
    #[must_use]
    pub fn msc500(workspace_root: &Path) -> Self {
        Self {
            name: "msc",
            archive_path: workspace_root.join("MSC500.zip"),
            extract_root: workspace_root.join(".msc500"),
            root_subdir: PathBuf::new(),
            sentinel: PathBuf::from("BIN/CL.EXE"),
        }
    }

    fn layout(&self) -> DistroLayout {
        let root_dir = self.extract_root.join(&self.root_subdir);
        DistroLayout {
            bin: root_dir.join("BIN"),
            include: root_dir.join("INCLUDE"),
            lib: root_dir.join("LIB"),
            root_dir,
        }
    }

    fn is_complete(&self, layout: &DistroLayout) -> bool {
        layout.root_dir.join(&self.sentinel).is_file()
    }
}

/// Extract the distribution's archive into its `extract_root/<root_subdir>/`
/// if it hasn't been already. Idempotent and safe to call concurrently:
/// the sentinel check guards re-entry.
pub fn ensure_extracted(spec: &DistroSpec) -> Result<DistroLayout, OracleError> {
    let layout = spec.layout();
    if spec.is_complete(&layout) {
        return Ok(layout);
    }
    if !spec.archive_path.is_file() {
        return Err(OracleError::ArchiveMissing(spec.archive_path.clone()));
    }

    fs::create_dir_all(&spec.extract_root)?;
    extract_zip(&spec.archive_path, &spec.extract_root)?;

    if !spec.is_complete(&layout) {
        return Err(OracleError::Io(io::Error::other(format!(
            "extraction finished but sentinel {} not present",
            layout.root_dir.join(&spec.sentinel).display()
        ))));
    }
    Ok(layout)
}

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), OracleError> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else { continue };
        let out_path = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&out_path)?;
        io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}
