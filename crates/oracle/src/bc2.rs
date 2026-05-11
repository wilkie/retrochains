//! BC2.zip extraction. Idempotent: a presence sentinel inside the extracted
//! tree (the BCC.EXE binary) is used to detect a complete prior extraction.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::OracleError;

/// Resolved paths inside an extracted BC2 install tree.
#[derive(Debug, Clone)]
pub struct Bc2Layout {
    /// `<root>/BC2` — the directory unpacked from the archive.
    pub bc2_dir: PathBuf,
    /// `<root>/BC2/BIN` — contains `BCC.EXE`, `TASM.EXE`, `TLINK.EXE`.
    pub bin: PathBuf,
    /// `<root>/BC2/INCLUDE` — system headers (the moral equivalent of
    /// `D:\BC2\INCLUDE` on a 1991 install).
    pub include: PathBuf,
    /// `<root>/BC2/LIB` — startup objects and runtime libraries per memory model.
    pub lib: PathBuf,
}

impl Bc2Layout {
    fn from_extracted(bc2_root: &Path) -> Self {
        let bc2_dir = bc2_root.join("BC2");
        Self {
            bin: bc2_dir.join("BIN"),
            include: bc2_dir.join("INCLUDE"),
            lib: bc2_dir.join("LIB"),
            bc2_dir,
        }
    }

    /// Has the archive been fully unpacked? We use the existence of BCC.EXE as
    /// the sentinel — it's the largest binary and is written last in our
    /// extraction loop.
    fn is_complete(&self) -> bool {
        self.bin.join("BCC.EXE").is_file()
    }
}

/// Extract `BC2.zip` into `<bc2_root>/BC2/` if not already there. Returns the
/// resolved layout. Idempotent and safe to call repeatedly.
pub fn ensure_extracted(bc2_zip: &Path, bc2_root: &Path) -> Result<Bc2Layout, OracleError> {
    let layout = Bc2Layout::from_extracted(bc2_root);
    if layout.is_complete() {
        return Ok(layout);
    }
    if !bc2_zip.is_file() {
        return Err(OracleError::Bc2ZipMissing(bc2_zip.to_path_buf()));
    }

    fs::create_dir_all(bc2_root)?;
    extract_zip(bc2_zip, bc2_root)?;

    if !layout.is_complete() {
        return Err(OracleError::Io(io::Error::other(format!(
            "extraction finished but BCC.EXE not present at {}",
            layout.bin.join("BCC.EXE").display()
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
