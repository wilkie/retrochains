//! `cl` driver — Phase 1 Slice 1 stub. Accepts the MSC 5.0
//! command-line shape `cl /c /AS <source>` and emits a hardcoded OBJ
//! matching what the real CL.EXE produces for that invocation. No
//! parser, no codegen — see `crates/msc/src/lib.rs` and
//! `specs/plans/MSC_PHASE_1.md`.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("msc: {e}");
            ExitCode::from(1)
        }
    }
}

fn try_main() -> Result<(), Box<dyn std::error::Error>> {
    let mut source: Option<PathBuf> = None;
    let mut saw_c = false;
    let mut saw_as = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "/c" | "-c" => saw_c = true,
            "/AS" | "-AS" => saw_as = true,
            other if other.starts_with('/') || other.starts_with('-') => {
                return Err(format!("unrecognized flag {other:?}").into());
            }
            other => {
                if source.is_some() {
                    return Err("multiple source files not yet supported".into());
                }
                source = Some(PathBuf::from(other));
            }
        }
    }
    if !saw_c {
        return Err("Phase 1 Slice 1 only supports compile-only `/c`".into());
    }
    if !saw_as {
        return Err("Phase 1 Slice 1 only supports small model `/AS`".into());
    }
    let source = source.ok_or("missing source-file argument")?;

    // CL echoes the source filename (uppercased) to stdout with CRLF
    // before starting compilation. Match its behavior so captured
    // stdouts compare byte-for-byte. Fixture 4075.
    let source_basename = source
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    print!("{source_basename}\r\n");

    msc::emit_dash_c(&source)?;
    Ok(())
}
