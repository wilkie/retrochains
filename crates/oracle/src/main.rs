//! Ad-hoc CLI for driving the oracle from the shell. Useful while developing.
//!
//! Usage:
//!     oracle <tool> [args...] -- <input-file>...
//!
//! `<tool>` is one of `bcc`, `tasm`, `tlink`. Each `<input-file>` is copied
//! into the DOS working directory under its basename (uppercased). Anything
//! produced by the tool is written next to stdout as `<NAME>` lines, and the
//! captured stdout from the tool is printed to our stdout verbatim.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use oracle::provision::{self, ProvisionSpec};
use oracle::{Oracle, OracleConfig, OracleInvocation, Tool};

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => ExitCode::from(u8::try_from(code & 0xFF).unwrap_or(255)),
        Err(e) => {
            eprintln!("oracle: {e}");
            ExitCode::from(2)
        }
    }
}

fn try_main() -> Result<i32, Box<dyn std::error::Error>> {
    let mut argv = std::env::args().skip(1);
    let tool_arg = argv.next().ok_or("missing <tool>")?;
    if tool_arg == "provision" {
        return cmd_provision(argv.collect());
    }
    let tool = match tool_arg.as_str() {
        "bcc" | "BCC" => Tool::Bcc,
        "tasm" | "TASM" => Tool::Tasm,
        "tlink" | "TLINK" => Tool::Tlink,
        other => return Err(format!("unknown tool: {other}").into()),
    };

    let mut tool_args = Vec::new();
    let mut input_paths = Vec::new();
    let mut seen_separator = false;
    for arg in argv {
        if !seen_separator && arg == "--" {
            seen_separator = true;
            continue;
        }
        if seen_separator {
            input_paths.push(arg);
        } else {
            tool_args.push(arg);
        }
    }

    let workspace_root = find_workspace_root()?;
    let cfg = OracleConfig::for_workspace(&workspace_root);
    let oracle = Oracle::open(cfg)?;

    let mut inputs_bytes = Vec::new();
    for path in &input_paths {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("reading input {path}: {e}"))?;
        inputs_bytes.push((dos_name_for(path), bytes));
    }
    // The DOS-uppercase filenames of materialized inputs are appended as
    // arguments after the tool's own flags — that's how BCC/TASM/TLINK actually
    // expect filenames on their command line.
    for (name, _) in &inputs_bytes {
        tool_args.push(name.clone());
    }
    let mut invocation = OracleInvocation::new(tool).args(tool_args);
    for (name, bytes) in &inputs_bytes {
        invocation = invocation.input(name.clone(), bytes.as_slice());
    }

    let run = oracle.run(&invocation)?;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(&run.stdout)?;
    // Materialize every output file the oracle produced next to the cwd, so
    // a `oracle bcc ... -- foo.c` run leaves FOO.OBJ on disk like the real tool
    // would. The CLI is intentionally simple — callers that want richer control
    // should use the library API directly.
    for (name, out) in &run.outputs {
        std::fs::write(name, &out.bytes)
            .map_err(|e| format!("writing output {name}: {e}"))?;
        eprintln!("[oracle] wrote {name} ({} bytes)", out.bytes.len());
    }
    Ok(run.exit_code)
}

/// Find the workspace root by walking up from cwd looking for the tracked
/// `oracles/` directory (the oracle archives live under it, e.g.
/// `oracles/bcc/BC2.zip`). Falls back to cwd if not found so the error path can
/// still surface a useful "archive not found at <X>" message from the library.
fn find_workspace_root() -> std::io::Result<std::path::PathBuf> {
    let cwd = std::env::current_dir()?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join("oracles").is_dir() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(cwd),
        }
    }
}

/// `oracle provision <bcc|msc> [SUBCOMMAND]` — rebuild a gitignored compiler
/// archive from its tracked descriptor + manifest. Subcommands let the
/// host-side stages run on their own while the DOSBox-X install step matures:
///
///   verify <tree-dir>     hash a tree against the manifest
///   repackage <tree-dir>  verify, then seal the canonical archive
///   fetch                 download + unpack the install media, list disk images
///   (none)                run the full pipeline
fn cmd_provision(args: Vec<String>) -> Result<i32, Box<dyn std::error::Error>> {
    let mut it = args.into_iter();
    let usage = "usage: oracle provision <bcc|msc> [verify <dir> | repackage <dir> | fetch]";
    let name = it.next().ok_or(usage)?;
    let root = find_provision_root()?;
    let spec = ProvisionSpec::for_name(&name, &root)
        .ok_or_else(|| format!("unknown distribution: {name} (expected bcc or msc)"))?;

    match it.next().as_deref() {
        Some("verify") => {
            let dir = it.next().ok_or("usage: oracle provision <name> verify <tree-dir>")?;
            let report = provision::verify_tree(Path::new(&dir), &spec.manifest_path)?;
            println!("{}", report.summary());
            Ok(i32::from(!report.is_ok()))
        }
        Some("repackage") => {
            let dir =
                it.next().ok_or("usage: oracle provision <name> repackage <tree-dir> [out.zip]")?;
            // Optional explicit output; defaults to the canonical archive path.
            let out = it.next().map_or_else(|| spec.distro.archive_path.clone(), PathBuf::from);
            // Never seal an archive we can't vouch for: verify first.
            let report = provision::verify_tree(Path::new(&dir), &spec.manifest_path)?;
            if !report.is_ok() {
                eprintln!("{}", report.summary());
                return Err("refusing to repackage: tree does not match the manifest".into());
            }
            provision::repackage(Path::new(&dir), &spec.manifest_path, &out)?;
            println!("wrote {} ({} files verified)", out.display(), report.checked);
            Ok(0)
        }
        Some("fetch") => {
            let cache = provision_cache(&root, &spec);
            let media = fetch_and_unpack(&spec, &cache)?;
            println!("media: {}", media.display());
            for img in provision::disk_images(&cache)? {
                println!("  disk image: {}", img.display());
            }
            Ok(0)
        }
        Some(other) => Err(format!("unknown provision subcommand: {other}").into()),
        None => {
            let cache = provision_cache(&root, &spec);
            let descriptor = provision::Descriptor::load(&spec.descriptor_path)?;
            let recipe = descriptor.install.as_ref().ok_or_else(|| {
                format!("descriptor {} has no [install] recipe", spec.descriptor_path.display())
            })?;
            fetch_and_unpack(&spec, &cache)?;

            let staging = cache.join("staging");
            if staging.exists() {
                std::fs::remove_dir_all(&staging)?;
            }
            std::fs::create_dir_all(&staging)?;
            eprintln!("[provision] assembling tree from install media …");
            provision::install_tree(recipe, &cache, &staging)?;

            let tree_base = &staging;
            let report = provision::verify_tree(tree_base, &spec.manifest_path)?;
            if !report.is_ok() {
                eprintln!("{}", report.summary());
                return Err("assembled tree does not match the manifest".into());
            }
            let out = &spec.distro.archive_path;
            provision::repackage(tree_base, &spec.manifest_path, out)?;
            println!(
                "provisioned {} → {} ({} files verified against the manifest)",
                spec.name,
                out.display(),
                report.checked
            );
            Ok(0)
        }
    }
}

fn fetch_and_unpack(
    spec: &ProvisionSpec,
    cache: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let descriptor = provision::Descriptor::load(&spec.descriptor_path)?;
    eprintln!("[provision] fetching media for {} …", spec.name);
    let media = provision::fetch_media(&descriptor, cache)?;
    eprintln!("[provision] unpacking {} …", media.display());
    provision::unpack_media(&media, cache)?;
    Ok(media)
}

fn provision_cache(root: &Path, spec: &ProvisionSpec) -> PathBuf {
    root.join(format!(".provision-{}", spec.name))
}

/// Find the workspace root for provisioning by walking up from cwd looking for
/// the tracked `oracles/` directory. Unlike `find_workspace_root`, this can't
/// key off `BC2.zip` — recreating that archive is the whole point.
fn find_provision_root() -> std::io::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join("oracles").is_dir() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(cwd),
        }
    }
}

fn dos_name_for(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_uppercase(), |s| s.to_string_lossy().to_uppercase())
}
