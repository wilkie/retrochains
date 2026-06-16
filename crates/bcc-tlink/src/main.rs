//! `tlink` — Turbo Link 4.0 command-line front end.
//!
//! TLINK's command line is `tlink [options] objfiles, exefile, mapfile,
//! libfiles`: comma-delimited fields, with object (and library) files joined
//! by `+` within a field. Options begin with `/` or `-`. We implement the
//! subset the standalone-linker fixtures exercise; unknown options are
//! accepted and ignored so a richer command line still links.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("tlink: {e}");
            ExitCode::from(1)
        }
    }
}

fn try_main() -> Result<(), Box<dyn std::error::Error>> {
    // Options are ignored for now; positional tokens form the comma-delimited
    // field string (DOS lets spaces split it, so concatenate the tokens).
    let mut positional = String::new();
    for arg in std::env::args().skip(1) {
        if arg.starts_with('/') || arg.starts_with('-') {
            continue;
        }
        positional.push_str(&arg);
    }
    if positional.is_empty() {
        return Err("no object files given".into());
    }

    let mut fields = positional.split(',');
    let obj_field = fields.next().unwrap_or("").trim();
    let exe_field = fields.next().unwrap_or("").trim();
    let map_field = fields.next().unwrap_or("").trim(); // fields[2] = map file
    let lib_field = fields.next().unwrap_or("").trim(); // fields[3] = libraries

    let obj_names: Vec<String> = obj_field
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(with_obj_ext)
        .collect();
    if obj_names.is_empty() {
        return Err("no object files given".into());
    }

    let mut objects = Vec::with_capacity(obj_names.len());
    for name in &obj_names {
        let bytes = std::fs::read(name).map_err(|e| format!("reading {name}: {e}"))?;
        objects.push((name.clone(), bytes));
    }

    // Libraries (`+`-joined, default extension `.LIB`).
    let mut libraries = Vec::new();
    for name in lib_field.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        let name = with_ext(name, "LIB");
        let bytes = std::fs::read(&name).map_err(|e| format!("reading {name}: {e}"))?;
        libraries.push((name, bytes));
    }

    let exe_path = if exe_field.is_empty() {
        default_exe_name(&obj_names[0])
    } else {
        with_ext(exe_field, "EXE")
    };

    let image = bcc_tlink::link_image(&objects, &libraries)?;
    std::fs::write(&exe_path, bcc_tlink::mz::write(&image))
        .map_err(|e| format!("writing {exe_path}: {e}"))?;

    // A `.MAP` listing is written whenever the map field names one.
    if !map_field.is_empty() {
        let map_path = with_ext(map_field, "MAP");
        std::fs::write(&map_path, bcc_tlink::map::format(&image))
            .map_err(|e| format!("writing {map_path}: {e}"))?;
    }
    Ok(())
}

/// Append `.OBJ` if the name has no extension (TLINK's default).
fn with_obj_ext(name: &str) -> String {
    with_ext(name, "OBJ")
}

fn with_ext(name: &str, ext: &str) -> String {
    if Path::new(name).extension().is_some() {
        name.to_owned()
    } else {
        format!("{name}.{ext}")
    }
}

/// EXE named after the first object's basename when no output field is given.
fn default_exe_name(first_obj: &str) -> String {
    let stem = PathBuf::from(first_obj)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("OUTPUT")
        .to_owned();
    format!("{stem}.EXE")
}
