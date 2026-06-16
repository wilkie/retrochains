//! `tlib` — Turbo Librarian 2.0 command-line front end.
//!
//! `tlib libname [operations]`: the first token names the library (default
//! extension `.LIB`); each `+module` operation adds `module.OBJ` as a member.
//! We implement library *creation* (`+` only) — enough to build the library
//! inputs the linker's `.LIB` fixtures need; `-`/`*`/`-+` come later.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("tlib: {e}");
            ExitCode::from(1)
        }
    }
}

fn try_main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let lib_arg = args.next().ok_or("usage: tlib libname [+module ...]")?;
    let lib_path = with_ext(&lib_arg, "LIB");

    let mut objects = Vec::new();
    let mut extended = false;
    for op in args {
        // `/E` requests an extended dictionary; other `/`,`-` options are ignored.
        if op.eq_ignore_ascii_case("/E") || op.eq_ignore_ascii_case("-E") {
            extended = true;
            continue;
        }
        if op.starts_with('/') || op.starts_with('-') {
            continue;
        }
        let Some(module) = op.strip_prefix('+') else {
            return Err(format!("unsupported operation {op:?} (only +module is implemented)").into());
        };
        // `+ADD` adds ADD.OBJ as module ADD (uppercased, no extension).
        let name = module_name(module);
        let obj_path = with_ext(module, "OBJ");
        let bytes = std::fs::read(&obj_path).map_err(|e| format!("reading {obj_path}: {e}"))?;
        objects.push((name, bytes));
    }
    if objects.is_empty() {
        return Err("no +module operations given".into());
    }

    let lib = bcc_tlib::build_library(&objects, extended)?;
    std::fs::write(&lib_path, &lib).map_err(|e| format!("writing {lib_path}: {e}"))?;
    Ok(())
}

/// Module name from a `+module` spec: the basename, no extension, uppercased.
fn module_name(spec: &str) -> String {
    PathBuf::from(spec)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(spec)
        .to_ascii_uppercase()
}

fn with_ext(name: &str, ext: &str) -> String {
    if Path::new(name).extension().is_some() {
        name.to_owned()
    } else {
        format!("{name}.{ext}")
    }
}
