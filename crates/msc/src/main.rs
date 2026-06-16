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
    let mut sources: Vec<PathBuf> = Vec::new();
    let mut saw_c = false;
    let mut saw_as = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "/c" | "-c" => saw_c = true,
            "/AS" | "-AS" => saw_as = true,
            // No-op flags. `/Fa` asks the oracle for an assembly
            // listing; our binary doesn't produce one yet (the ASM
            // tier is advisory in verify-all). The single-letter
            // diagnostic / preprocessor flags are also harmless to
            // skip — we don't honor them but never reject them.
            "/Fa" | "-Fa" | "/Fc" | "-Fc" => { /* listing flags ignored */ }
            "/Zg" | "-Zg" | "/Zl" | "-Zl" | "/Zp" | "-Zp" => { /* misc no-ops */ }
            other if other.starts_with('/') || other.starts_with('-') => {
                return Err(format!("unrecognized flag {other:?}").into());
            }
            other => sources.push(PathBuf::from(other)),
        }
    }
    if !saw_c {
        return Err("Phase 1 Slice 1 only supports compile-only `/c`".into());
    }
    if !saw_as {
        return Err("Phase 1 Slice 1 only supports small model `/AS`".into());
    }
    if sources.is_empty() {
        return Err("missing source-file argument".into());
    }

    // CL compiles each source independently into its own OBJ (each is a
    // separate translation unit). It echoes the source filename (uppercased)
    // to stdout with CRLF before compiling it; with several sources it echoes
    // each in turn. Match that so captured stdouts compare byte-for-byte
    // (fixture 4075). Multi-source links via LINK, which we don't reimplement —
    // the `linking/` fixtures gate on the per-TU OBJs and treat the .EXE/.MAP
    // as advisory.
    for source in &sources {
        let source_basename = source
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        print!("{source_basename}\r\n");
        msc::emit_dash_c(source)?;
    }
    Ok(())
}
