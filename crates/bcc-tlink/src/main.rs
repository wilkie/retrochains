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

    // The object field is `+`/space-separated module names; a positional `/o`
    // token turns on overlay mode, so every module after it is overlaid (TLINK
    // 4.0's overlay selection — not the parenthesis form). `/o-` would turn it
    // back off.
    let mut obj_names: Vec<String> = Vec::new();
    let mut overlaid: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut overlay_mode = false;
    for token in obj_field.split(['+', ' ', '\t']).map(str::trim).filter(|s| !s.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "/o" => overlay_mode = true,
            "/o-" => overlay_mode = false,
            _ => {
                let name = with_obj_ext(token);
                if overlay_mode {
                    overlaid.insert(name.clone());
                }
                obj_names.push(name);
            }
        }
    }
    if obj_names.is_empty() {
        return Err("no object files given".into());
    }
    // Overlay linking (the VROOMM manager, per-module INT 3F stubs, the
    // _EXEINFO_ table, and the appended FBOV overlay area) is reverse-engineered
    // (see specs/bcc/tlink/OVERLAYS.md) but not yet built. Fail clearly rather
    // than silently producing a non-overlay image.
    if !overlaid.is_empty() {
        let mut names: Vec<&String> = overlaid.iter().collect();
        names.sort();
        return Err(format!(
            "overlay linking (/o) not yet implemented; overlaid modules: {}",
            names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        )
        .into());
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
