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
use std::process::ExitCode;

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
    for (name, bytes) in &run.outputs {
        std::fs::write(name, bytes)
            .map_err(|e| format!("writing output {name}: {e}"))?;
        eprintln!("[oracle] wrote {name} ({} bytes)", bytes.len());
    }
    Ok(run.exit_code)
}

/// Find the workspace root by walking up from cwd looking for BC2.zip. Falls
/// back to cwd if not found so the error path can still surface a useful
/// "BC2.zip not found at <X>" message from the library layer.
fn find_workspace_root() -> std::io::Result<std::path::PathBuf> {
    let cwd = std::env::current_dir()?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join("BC2.zip").is_file() {
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
