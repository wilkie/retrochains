//! Rebuild an oracle distribution from its *original install media*.
//!
//! The compiler archives the oracle drives (`BC2.zip`, `MSC500.zip`) are
//! gitignored — not ours to redistribute. What *is* tracked is, per
//! distribution, an `oracles/<c>/<NAME>.toml` descriptor (where the media can
//! be downloaded) and an `oracles/<c>/<NAME>.sha256` manifest (the byte-exact
//! hash of every file in the assembled tree). This module lets a developer or
//! CI job *re-derive* the gitignored archive from those two tracked files.
//!
//! The pipeline runs in five stages:
//!
//! - **fetch** resolves the WinWorld landing page to a direct media URL and
//!   downloads it, gated on the descriptor's `archive_sha256`.
//! - **unpack** expands the media (a 7z of the original install floppies) into
//!   its `DISK*.IMG` images.
//! - **install** drives the vendor installer to assemble the tree. The media
//!   stores everything as split archives that only the installer can join and
//!   build (startup objects are assembled with TASM at install time), so this
//!   stage runs the real `INSTALL.EXE`/`SETUP` under DOSBox-X.
//! - **verify** hashes every file in the assembled tree against the committed
//!   `.sha256` manifest. This is the correctness gate: the install stage is
//!   "right" exactly when all hashes reproduce.
//! - **repackage** zips the verified tree into the canonical archive at the
//!   workspace root so the normal lazy-extract path (`distro::ensure_extracted`)
//!   works unchanged afterward.
//!
//! The fetch, unpack, verify, and repackage stages are pure host-side work and
//! live here. The install stage needs DOSBox-X and is the only piece that
//! touches a DOS environment.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::distro::DistroSpec;

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("reading descriptor {0}: {1}")]
    Descriptor(PathBuf, io::Error),
    #[error("parsing descriptor {0}: {1}")]
    DescriptorParse(PathBuf, toml::de::Error),
    #[error("reading manifest {0}: {1}")]
    Manifest(PathBuf, io::Error),
    #[error("manifest {0} line {1} is not in `<sha256>  <path>` form: {2:?}")]
    ManifestLine(PathBuf, usize, String),
    #[error("running curl ({0}): {1}")]
    CurlSpawn(String, io::Error),
    #[error("curl failed for {url} (exit {code}): {stderr}")]
    Curl { url: String, code: i32, stderr: String },
    #[error("could not find a download mirror on the WinWorld page {0}")]
    NoMirror(String),
    #[error(
        "downloaded media sha256 mismatch:\n  expected {expected}\n  actual   {actual}\n  (descriptor `archive_sha256` and the upstream file disagree)"
    )]
    MediaHash { expected: String, actual: String },
    #[error("descriptor has no `archive_sha256`; refusing to trust an unpinned download")]
    NoMediaHash,
    #[error("extracting media {0}: {1}")]
    Unpack(PathBuf, sevenz_rust2::Error),
    #[error("descriptor {0} has no [install] recipe")]
    NoRecipe(PathBuf),
    #[error("reading floppy image {0}: {1}")]
    FatImage(PathBuf, String),
    #[error("install archive not found in media: {0}")]
    ArchiveMissing(String),
    #[error("running {tool} ({args}): {source}")]
    ToolSpawn { tool: &'static str, args: String, source: io::Error },
    #[error("{tool} failed (exit {code}) for {args}:\n{stderr}")]
    Tool { tool: &'static str, code: i32, args: String, stderr: String },
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// Parsed `oracles/<c>/<NAME>.toml` descriptor.
///
/// Extra keys are allowed (the file also documents `winworldpc_url`, `name`,
/// `version` for humans); only the fields provisioning needs are modeled.
#[derive(Debug, Clone, Deserialize)]
pub struct Descriptor {
    /// The WinWorld *landing* page for the release (an HTML page that lists
    /// per-mirror download links — not a direct file).
    pub winworldpc_download: String,
    /// sha256 of the upstream media archive (the `.7z`). The per-file manifest
    /// covers the *installed* tree; this covers the *downloaded media*, so a
    /// corrupt or swapped upstream file is caught before we ever unpack it.
    #[serde(default)]
    pub archive_sha256: Option<String>,
    /// Recipe for assembling the verified tree from the media's archives.
    #[serde(default)]
    pub install: Option<InstallRecipe>,
}

/// Declarative description of how the vendor installer's output is reproduced
/// from the raw install-media archives. Executed by [`install_tree`].
#[derive(Debug, Clone, Deserialize)]
pub struct InstallRecipe {
    /// Top-level directory of the produced tree (matches the manifest prefix,
    /// e.g. `BC2`). All step destinations are relative to it.
    pub tree_root: String,
    /// Plain archives expanded into a destination subdir.
    #[serde(default)]
    pub extract: Vec<ExtractStep>,
    /// Multi-volume archives joined then expanded.
    #[serde(default)]
    pub span: Vec<SpanStep>,
    /// Files relocated within the tree after extraction.
    #[serde(default)]
    pub relocate: Vec<RelocateStep>,
    /// Verbatim file copies out of the (uncompressed) floppy images. Used by
    /// distributions whose media isn't archive-based (e.g. MS C 5.0).
    #[serde(default)]
    pub copy: Vec<CopyStep>,
    /// Combined libraries built by running the vendor's `LIB.EXE` under DOSBox
    /// (the only non-copy step the MS C installer performs).
    #[serde(default)]
    pub lib_build: Vec<LibBuildStep>,
    /// Small files the installer generates rather than ships.
    #[serde(default)]
    pub write: Vec<WriteStep>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CopyStep {
    /// Disk-image stem the files live on (e.g. `INCLIBSM`).
    pub disk: String,
    /// Subdir within that disk (default: the image root).
    #[serde(default)]
    pub from: String,
    /// Destination subdir of `tree_root`.
    pub into: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LibBuildStep {
    /// Tree subdir the build runs in (e.g. `LIB`); also where `base`, the
    /// component libraries, and `output` reside.
    pub dir: String,
    /// Existing library copied to `output` as the starting point.
    pub base: String,
    /// Library to produce.
    pub output: String,
    /// `LIB.EXE` operations, e.g. `+LIBH.LIB +SLIBFP.LIB +87.LIB +GRAPHICS.LIB`.
    pub ops: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WriteStep {
    /// Tree-relative output path.
    pub path: String,
    /// Lines, joined with CRLF.
    pub lines: Vec<String>,
    /// Whether to append a trailing CRLF.
    #[serde(default)]
    pub trailing_newline: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtractStep {
    pub archives: Vec<String>,
    /// Destination subdir of `tree_root`.
    pub into: String,
    /// If set, only these members are extracted (the archive may carry tools
    /// the manifest doesn't ship).
    #[serde(default)]
    pub only: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpanStep {
    /// Volume files in order; concatenated then repaired with `zip -FF`.
    pub parts: Vec<String>,
    pub into: String,
    #[serde(default)]
    pub only: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelocateStep {
    /// Source subdir of `tree_root`.
    pub from: String,
    /// Destination subdir of `tree_root` (created if absent).
    pub into: String,
    pub files: Vec<String>,
}

impl Descriptor {
    /// Load and parse a descriptor TOML.
    ///
    /// # Errors
    /// If the file can't be read or isn't valid TOML for the modeled fields.
    pub fn load(path: &Path) -> Result<Self, ProvisionError> {
        let text =
            fs::read_to_string(path).map_err(|e| ProvisionError::Descriptor(path.to_path_buf(), e))?;
        toml::from_str(&text).map_err(|e| ProvisionError::DescriptorParse(path.to_path_buf(), e))
    }
}

/// Ties a compiler name to its tracked descriptor + manifest and the
/// `DistroSpec` that says where the rebuilt archive should land.
#[derive(Debug, Clone)]
pub struct ProvisionSpec {
    pub name: &'static str,
    pub descriptor_path: PathBuf,
    pub manifest_path: PathBuf,
    pub distro: DistroSpec,
}

impl ProvisionSpec {
    #[must_use]
    pub fn bcc(workspace_root: &Path) -> Self {
        Self {
            name: "bcc",
            descriptor_path: workspace_root.join("oracles/bcc/BC2.toml"),
            manifest_path: workspace_root.join("oracles/bcc/BC2.sha256"),
            distro: DistroSpec::bc2(workspace_root),
        }
    }

    #[must_use]
    pub fn msc(workspace_root: &Path) -> Self {
        Self {
            name: "msc",
            descriptor_path: workspace_root.join("oracles/msc/MSC500.toml"),
            manifest_path: workspace_root.join("oracles/msc/MSC500.sha256"),
            distro: DistroSpec::msc500(workspace_root),
        }
    }

    /// Resolve a provisioning spec from a `--compiler`-style name.
    pub fn for_name(name: &str, workspace_root: &Path) -> Option<Self> {
        match name {
            "bcc" | "BCC" | "bc2" => Some(Self::bcc(workspace_root)),
            "msc" | "MSC" | "msc500" => Some(Self::msc(workspace_root)),
            _ => None,
        }
    }
}

/// One `<sha256>  <relative/path>` line of a manifest.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub sha256: String,
    /// Path relative to the assembled tree's base directory (the directory the
    /// manifest hashes are taken relative to). Uses `/` separators.
    pub path: String,
}

/// Parse a `sha256sum`-format manifest (`<64 hex>␠␠<path>` per line). Blank
/// lines are skipped; the order of the file is preserved.
///
/// # Errors
/// If the file can't be read, or a non-blank line isn't `<64 hex>  <path>`.
pub fn parse_manifest(path: &Path) -> Result<Vec<ManifestEntry>, ProvisionError> {
    let text =
        fs::read_to_string(path).map_err(|e| ProvisionError::Manifest(path.to_path_buf(), e))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // `sha256sum` writes exactly two spaces between hash and path; the
        // second char may be `*` for binary mode. Accept either separator.
        let (hash, rest) = line
            .split_once(' ')
            .ok_or_else(|| ProvisionError::ManifestLine(path.to_path_buf(), i + 1, line.to_string()))?;
        let rel = rest.trim_start_matches([' ', '*']);
        if hash.len() != 64 || rel.is_empty() {
            return Err(ProvisionError::ManifestLine(path.to_path_buf(), i + 1, line.to_string()));
        }
        out.push(ManifestEntry { sha256: hash.to_ascii_lowercase(), path: rel.to_string() });
    }
    Ok(out)
}

/// Outcome of checking an assembled tree against a manifest.
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub checked: usize,
    /// Manifest paths absent from the tree.
    pub missing: Vec<String>,
    /// `(path, expected, actual)` for files whose bytes differ.
    pub mismatched: Vec<(String, String, String)>,
}

impl VerifyReport {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.missing.is_empty() && self.mismatched.is_empty()
    }

    /// A human-readable summary suitable for CLI output.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_ok() {
            return format!("OK — all {} files match the manifest", self.checked);
        }
        let mut s = format!(
            "FAIL — {} checked, {} missing, {} mismatched",
            self.checked,
            self.missing.len(),
            self.mismatched.len()
        );
        for m in &self.missing {
            let _ = write!(s, "\n  missing:    {m}");
        }
        for (p, e, a) in &self.mismatched {
            let _ = write!(s, "\n  mismatch:   {p}\n    expected {e}\n    actual   {a}");
        }
        s
    }
}

/// Hash every manifest file under `base_dir` and compare against the manifest.
/// `base_dir` is the directory the manifest paths are relative to (for BC2 the
/// staging dir that *contains* `BC2/…`; for MSC the dir that contains `BIN/…`).
///
/// # Errors
/// If the manifest can't be parsed, or a tree file can't be read (other than
/// being absent, which is reported as a mismatch in [`VerifyReport`]).
pub fn verify_tree(base_dir: &Path, manifest_path: &Path) -> Result<VerifyReport, ProvisionError> {
    let entries = parse_manifest(manifest_path)?;
    let mut report = VerifyReport::default();
    for entry in &entries {
        let file = base_dir.join(&entry.path);
        match fs::read(&file) {
            Ok(bytes) => {
                report.checked += 1;
                let got = hex_sha256(&bytes);
                if got != entry.sha256 {
                    report.mismatched.push((entry.path.clone(), entry.sha256.clone(), got));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => report.missing.push(entry.path.clone()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(report)
}

/// Zip exactly the manifest's files (read from `base_dir`) into `out_zip`,
/// preserving their manifest-relative paths. The manifest and the canonical
/// archive are a 1:1 set, so this *is* the whole distribution — no unverified
/// extras leak in. Parent directories are recreated on extract from the file
/// paths, so no explicit directory entries are written.
///
/// # Errors
/// If the manifest can't be parsed, a listed file can't be read, or the output
/// zip can't be written.
pub fn repackage(
    base_dir: &Path,
    manifest_path: &Path,
    out_zip: &Path,
) -> Result<(), ProvisionError> {
    let entries = parse_manifest(manifest_path)?;
    if let Some(parent) = out_zip.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(out_zip)?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for entry in &entries {
        let bytes = fs::read(base_dir.join(&entry.path))?;
        zw.start_file(&entry.path, opts)?;
        zw.write_all(&bytes)?;
    }
    zw.finish()?;
    Ok(())
}

/// Resolve the WinWorld landing page to a direct media URL, download it, and
/// verify it against the descriptor's `archive_sha256`. Returns the path to the
/// downloaded `.7z`. Tries each mirror the page lists until one downloads and
/// hashes correctly.
///
/// # Errors
/// If the descriptor has no `archive_sha256`, the landing page lists no mirror,
/// a `curl` invocation fails, or no mirror's bytes match the pinned hash.
pub fn fetch_media(
    descriptor: &Descriptor,
    dest_dir: &Path,
) -> Result<PathBuf, ProvisionError> {
    let Some(expected) = descriptor.archive_sha256.as_deref() else {
        return Err(ProvisionError::NoMediaHash);
    };
    let expected = expected.to_ascii_lowercase();
    fs::create_dir_all(dest_dir)?;
    let out = dest_dir.join("media.7z");

    // Reuse a previously downloaded media file if it still matches the pin —
    // re-provisioning shouldn't re-fetch 12 MB every time.
    if out.is_file() && hex_sha256(&fs::read(&out)?) == expected {
        return Ok(out);
    }

    let landing = &descriptor.winworldpc_download;
    let html = curl_text(landing)?;
    let mirrors = resolve_mirrors(landing, &html);
    if mirrors.is_empty() {
        return Err(ProvisionError::NoMirror(landing.clone()));
    }

    let mut last_actual = String::new();
    for mirror in &mirrors {
        curl_download(mirror, &out)?;
        let actual = hex_sha256(&fs::read(&out)?);
        if actual == expected {
            return Ok(out);
        }
        last_actual = actual;
    }
    Err(ProvisionError::MediaHash { expected, actual: last_actual })
}

/// Expand the downloaded media `.7z` into `out_dir`. The archive holds the
/// original install floppies (`DISK01.IMG …`) plus artwork; everything is
/// extracted and the caller picks out the disk images.
///
/// # Errors
/// If the output directory can't be created or the 7z can't be decompressed.
pub fn unpack_media(archive_7z: &Path, out_dir: &Path) -> Result<(), ProvisionError> {
    fs::create_dir_all(out_dir)?;
    sevenz_rust2::decompress_file(archive_7z, out_dir)
        .map_err(|e| ProvisionError::Unpack(archive_7z.to_path_buf(), e))
}

/// Assemble the verified tree from the unpacked install media, following the
/// descriptor's [`InstallRecipe`]. Reads every file out of the floppy images
/// into a scratch dir, then expands the recipe's archives into `<staging>/`.
/// The produced tree's root dir is `recipe.tree_root` (e.g. `<staging>/BC2`),
/// which is exactly what [`verify_tree`] and [`repackage`] expect as a base.
///
/// # Errors
/// If the recipe is absent, a floppy image or archive can't be read, or a
/// `unzip`/`zip` invocation fails.
pub fn install_tree(
    recipe: &InstallRecipe,
    unpacked_dir: &Path,
    staging: &Path,
) -> Result<(), ProvisionError> {
    // 1. Pull every file out of each FAT12 floppy image into `.media/<STEM>/`,
    //    preserving subdirectories. Per-disk so `copy` steps can name a disk and
    //    `extract`/`span` steps can search across disks for an archive.
    let media = staging.join(".media");
    if media.exists() {
        fs::remove_dir_all(&media)?;
    }
    fs::create_dir_all(&media)?;
    for img in disk_images(unpacked_dir)? {
        let stem = img
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().to_ascii_uppercase());
        let disk_dir = media.join(&stem);
        fs::create_dir_all(&disk_dir)?;
        extract_fat_image(&img, &disk_dir)?;
    }

    let root = staging.join(&recipe.tree_root);
    fs::create_dir_all(&root)?;

    // 2. Plain archives (search every disk for the named archive).
    for step in &recipe.extract {
        let dest = root.join(&step.into);
        fs::create_dir_all(&dest)?;
        for archive in &step.archives {
            let path = find_in_media(&media, archive)?;
            run_unzip(&path, &dest, &step.only)?;
        }
    }

    // 3. Multi-volume spans: concat the parts, repair, then extract.
    for step in &recipe.span {
        let dest = root.join(&step.into);
        fs::create_dir_all(&dest)?;
        let mut joined = Vec::new();
        for part in &step.parts {
            joined.extend(fs::read(find_in_media(&media, part)?)?);
        }
        let cat = media.join("_span_cat.zip");
        let fixed = media.join("_span_fixed.zip");
        fs::write(&cat, &joined)?;
        run_zip_fix(&cat, &fixed)?;
        run_unzip(&fixed, &dest, &step.only)?;
    }

    // 4. Verbatim copies from a named disk's subdir (uncompressed media).
    for step in &recipe.copy {
        let src = media.join(&step.disk).join(&step.from);
        let dest = root.join(&step.into);
        fs::create_dir_all(&dest)?;
        for file in &step.files {
            fs::copy(src.join(file), dest.join(file))?;
        }
    }

    // 5. Combined libraries built by the vendor LIB.EXE under DOSBox.
    if !recipe.lib_build.is_empty() {
        run_lib_builds(&recipe.lib_build, &root)?;
    }

    // 6. Files the installer generates rather than ships.
    for step in &recipe.write {
        let path = root.join(&step.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut content = step.lines.join("\r\n");
        if step.trailing_newline {
            content.push_str("\r\n");
        }
        fs::write(path, content)?;
    }

    // 7. Relocations the installer performs after extraction.
    for step in &recipe.relocate {
        let from = root.join(&step.from);
        let into = root.join(&step.into);
        fs::create_dir_all(&into)?;
        for file in &step.files {
            fs::copy(from.join(file), into.join(file))?;
        }
    }

    fs::remove_dir_all(&media)?;
    Ok(())
}

/// Extract every regular file from a FAT12 floppy image into `dest`, recursing
/// into subdirectories (MS C's floppies nest INCLUDE/, LIB/, etc.).
fn extract_fat_image(img: &Path, dest: &Path) -> Result<(), ProvisionError> {
    let bytes = fs::read(img)?;
    let cursor = io::Cursor::new(bytes);
    let fs_img = fatfs::FileSystem::new(cursor, fatfs::FsOptions::new())
        .map_err(|e| ProvisionError::FatImage(img.to_path_buf(), e.to_string()))?;
    let err = |e: std::io::Error| ProvisionError::FatImage(img.to_path_buf(), e.to_string());
    extract_fat_dir(&fs_img.root_dir(), dest, &err)
}

fn extract_fat_dir<T: fatfs::ReadWriteSeek>(
    dir: &fatfs::Dir<'_, T>,
    dest: &Path,
    err: &impl Fn(std::io::Error) -> ProvisionError,
) -> Result<(), ProvisionError> {
    fs::create_dir_all(dest)?;
    for entry in dir.iter() {
        let entry = entry.map_err(err)?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        if entry.is_dir() {
            extract_fat_dir(&entry.to_dir(), &dest.join(&name), err)?;
        } else {
            let mut file = entry.to_file();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut buf).map_err(err)?;
            fs::write(dest.join(&name), buf)?;
        }
    }
    Ok(())
}

/// Find an archive by basename anywhere in the per-disk media tree.
fn find_in_media(media: &Path, name: &str) -> Result<PathBuf, ProvisionError> {
    for entry in fs::read_dir(media)? {
        let disk = entry?.path();
        if disk.is_dir() {
            let candidate = disk.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(ProvisionError::ArchiveMissing(name.to_string()))
}

/// `unzip -o <archive> [members…] -d <dest>`.
fn run_unzip(archive: &Path, dest: &Path, only: &[String]) -> Result<(), ProvisionError> {
    let mut cmd = Command::new("unzip");
    cmd.arg("-o").arg(archive);
    for member in only {
        cmd.arg(member);
    }
    cmd.arg("-d").arg(dest);
    run_tool("unzip", cmd, &format!("{}", archive.display()))
}

/// `zip -FF <broken> --out <fixed>` to reconstruct a concatenated multi-volume
/// archive's central directory.
fn run_zip_fix(broken: &Path, fixed: &Path) -> Result<(), ProvisionError> {
    let mut cmd = Command::new("zip");
    cmd.arg("-FF").arg(broken).arg("--out").arg(fixed);
    run_tool("zip", cmd, &format!("{}", broken.display()))
}

fn run_tool(tool: &'static str, mut cmd: Command, args: &str) -> Result<(), ProvisionError> {
    let output = cmd
        .output()
        .map_err(|e| ProvisionError::ToolSpawn { tool, args: args.to_string(), source: e })?;
    // `zip -FF` exits non-zero (2) even on a successful reconstruction, so the
    // real gate is the manifest verification downstream, not these exit codes.
    // We still surface a spawn failure (tool missing) above.
    if !output.status.success() && tool != "zip" {
        return Err(ProvisionError::Tool {
            tool,
            code: output.status.code().unwrap_or(-1),
            args: args.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Build each combined library by driving the vendor `LIB.EXE` under DOSBox-X,
/// one DOSBox invocation per library (batching all of them in a single session
/// can intermittently truncate the last output). The produced tree's BIN holds
/// `LIB.EXE` and its `dir` holds the component libraries, so the whole build
/// runs against the freshly-copied tree mounted as `C:`.
fn run_lib_builds(steps: &[LibBuildStep], root: &Path) -> Result<(), ProvisionError> {
    let mount = root.canonicalize()?;
    for step in steps {
        let dir = step.dir.replace('/', "\\");
        let bak = format!("{}.BAK", step.output.rsplit_once('.').map_or(&*step.output, |(s, _)| s));
        let bat = format!(
            "@echo off\r\nc:\r\npath c:\\bin\r\ncd \\{dir}\r\n\
             del {out}\r\ncopy {base} {out}\r\nLIB {out} {ops};\r\ndel {bak}\r\nexit\r\n",
            out = step.output,
            base = step.base,
            ops = step.ops,
        );
        let bat_path = root.join("_LIBBLD.BAT");
        fs::write(&bat_path, bat)?;
        run_dosbox_x(&mount, "_LIBBLD.BAT")?;
        let _ = fs::remove_file(&bat_path);
    }
    Ok(())
}

/// Run a `.BAT` headlessly under DOSBox-X with `mount_c` as drive `C:`. The
/// DOSBox-X command is taken from `$ORACLE_DOSBOX_X` (whitespace-split) and
/// defaults to the Flathub build with dummy SDL drivers. Exit status isn't
/// gated — the manifest verification downstream is the real correctness check.
fn run_dosbox_x(mount_c: &Path, bat: &str) -> Result<(), ProvisionError> {
    const DEFAULT: &str = "flatpak run --env=SDL_VIDEODRIVER=dummy \
        --env=SDL_AUDIODRIVER=dummy com.dosbox_x.DOSBox-X";
    let spec = std::env::var("ORACLE_DOSBOX_X").unwrap_or_else(|_| DEFAULT.to_string());
    let mut words = spec.split_whitespace();
    let prog = words.next().unwrap_or("dosbox-x");
    let mut cmd = Command::new(prog);
    cmd.args(words)
        .env("SDL_VIDEODRIVER", "dummy")
        .env("SDL_AUDIODRIVER", "dummy")
        .arg("-silent")
        .arg("-exit")
        .arg("-c")
        .arg(format!("mount c \"{}\"", mount_c.display()))
        .arg("-c")
        .arg("c:")
        .arg("-c")
        .arg(bat)
        .arg("-c")
        .arg("exit");
    cmd.output()
        .map_err(|e| ProvisionError::ToolSpawn { tool: "dosbox-x", args: spec, source: e })?;
    Ok(())
}

/// Collect the floppy disk images produced by `unpack_media`, sorted by name
/// (`DISK01.IMG`, `DISK02.IMG`, …) so installers see them in order.
///
/// # Errors
/// If a directory in the unpacked tree can't be read.
pub fn disk_images(unpacked_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut imgs = BTreeMap::new();
    collect_imgs(unpacked_dir, &mut imgs)?;
    Ok(imgs.into_values().collect())
}

fn collect_imgs(dir: &Path, out: &mut BTreeMap<String, PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_imgs(&path, out)?;
            continue;
        }
        let is_img = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("img"));
        if is_img {
            let name = entry.file_name().to_string_lossy().to_ascii_uppercase();
            out.insert(name, path);
        }
    }
    Ok(())
}

// --- helpers ---------------------------------------------------------------

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Pull the per-mirror download links out of a WinWorld landing page. They look
/// like `href="/download/<uuid>/from/<mirror-uuid>"`; we resolve them against
/// the page's origin into absolute URLs.
fn resolve_mirrors(landing_url: &str, html: &str) -> Vec<String> {
    let origin = origin_of(landing_url);
    let mut out = Vec::new();
    let needle = "/download/";
    let mut rest = html;
    while let Some(pos) = rest.find(needle) {
        let tail = &rest[pos..];
        let end = tail.find(['"', '\'', ' ', '>']).unwrap_or(tail.len());
        let href = &tail[..end];
        if href.contains("/from/") {
            let abs = format!("{origin}{href}");
            if !out.contains(&abs) {
                out.push(abs);
            }
        }
        rest = &tail[needle.len()..];
    }
    out
}

/// `https://host[:port]` of a URL, for resolving root-relative hrefs.
fn origin_of(url: &str) -> String {
    let (scheme, after) = url.split_once("://").unwrap_or(("https", url));
    let host = after.split('/').next().unwrap_or(after);
    format!("{scheme}://{host}")
}

/// `curl -sL` returning the body as text (for the HTML landing page).
fn curl_text(url: &str) -> Result<String, ProvisionError> {
    let output = curl_base()
        .arg(url)
        .output()
        .map_err(|e| ProvisionError::CurlSpawn(url.to_string(), e))?;
    if !output.status.success() {
        return Err(ProvisionError::Curl {
            url: url.to_string(),
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `curl -sL -o <out>` following redirects to the final mirror file.
fn curl_download(url: &str, out: &Path) -> Result<(), ProvisionError> {
    let output = curl_base()
        .arg("-o")
        .arg(out)
        .arg(url)
        .output()
        .map_err(|e| ProvisionError::CurlSpawn(url.to_string(), e))?;
    if !output.status.success() {
        return Err(ProvisionError::Curl {
            url: url.to_string(),
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

fn curl_base() -> Command {
    let mut cmd = Command::new("curl");
    // -s quiet, -L follow redirects, -f fail on HTTP errors, a real UA so the
    // CDN doesn't serve a bot page, and a generous cap for the ~12 MB media.
    cmd.arg("-sLf")
        .arg("--max-time")
        .arg("300")
        .arg("-A")
        .arg("Mozilla/5.0");
    cmd
}
