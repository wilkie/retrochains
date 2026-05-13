//! `xfingerprint` — classify OBJ/LIB files by toolchain.
//!
//! Usage:
//!     xfingerprint [--json] [--verbose] <file>...
//!
//! Default output is a one-line headline per file, plus per-member
//! breakdowns for LIB archives. `--verbose` adds the structural
//! evidence for each module (LNAMES contents, SEGDEF bytes, first
//! code bytes). `--json` swaps to machine-readable output.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use fingerprint::{analyze, Analysis, FingerprintTier, LibAnalysis, ObjAnalysis};

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xfingerprint: {e}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, Default)]
struct Args {
    paths: Vec<PathBuf>,
    json: bool,
    verbose: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--json" => a.json = true,
            "-v" | "--verbose" => a.verbose = true,
            "-h" | "--help" => {
                println!(
                    "usage: xfingerprint [--json] [--verbose] <file>...\n\
                     \n\
                     Classify OBJ and LIB files by toolchain fingerprint.\n\
                     Default: one-line per file. --verbose adds evidence.\n\
                     --json: machine-readable output."
                );
                std::process::exit(0);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag: {other}"));
            }
            path => a.paths.push(PathBuf::from(path)),
        }
    }
    if a.paths.is_empty() {
        return Err("no input files; pass <file>... or --help".into());
    }
    Ok(a)
}

fn try_main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let mut any_unknown = false;
    if args.json {
        print_json(&args, &mut any_unknown)?;
    } else {
        print_text(&args, &mut any_unknown)?;
    }
    Ok(if any_unknown {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    })
}

fn print_text(args: &Args, any_unknown: &mut bool) -> Result<(), Box<dyn std::error::Error>> {
    for path in &args.paths {
        let data = fs::read(path)?;
        let analysis = analyze(&data)?;
        println!("{}: {}", path.display(), analysis.headline());
        match &analysis {
            Analysis::Obj(o) => {
                if args.verbose {
                    print_obj_evidence(o, "  ");
                }
                if matches!(o.tier(), FingerprintTier::Unknown) {
                    *any_unknown = true;
                }
            }
            Analysis::Lib(l) => print_lib_members(l, args.verbose),
            Analysis::Unknown { .. } => {
                *any_unknown = true;
            }
        }
    }
    Ok(())
}

fn print_obj_evidence(o: &ObjAnalysis, indent: &str) {
    if let Some(name) = &o.name {
        println!("{indent}name: {name}");
    }
    println!(
        "{indent}signals: translator={}, EA={}, E8={}, A1={}, bcc-lnames={}, bcc-segdefs={}, bcc-grpdef={}, bcc-prologue={}",
        yn(o.has_bcc_translator()),
        yn(o.ea_marker.is_some()),
        yn(o.e8_trailer.is_some()),
        yn(o.a1_marker_empty),
        yn(o.matches_bcc_lnames()),
        yn(o.matches_bcc_segdefs()),
        yn(o.matches_bcc_grpdef()),
        yn(o.has_bcc_prologue()),
    );
    if let Some(names) = &o.first_lnames {
        let preview: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
        println!("{indent}lnames ({}): [{}]", names.len(), preview.join(", "));
    }
    if !o.first_code_bytes.is_empty() {
        let hex: Vec<String> = o.first_code_bytes.iter().map(|b| format!("{b:02x}")).collect();
        println!("{indent}first_code_bytes: {}", hex.join(" "));
    }
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn print_lib_members(l: &LibAnalysis, verbose: bool) {
    println!(
        "  page_size={}, dict@0x{:x} ({} blocks), flags=0x{:02x}",
        l.page_size, l.dictionary_offset, l.dictionary_blocks, l.flags
    );
    if !verbose {
        return;
    }
    for m in &l.members {
        let name = m.analysis.name.as_deref().unwrap_or("?");
        println!(
            "  @0x{:06x} {:32} {}",
            m.start_offset,
            name,
            m.analysis.tier().describe()
        );
    }
}

fn print_json(args: &Args, any_unknown: &mut bool) -> Result<(), Box<dyn std::error::Error>> {
    // No serde dependency for now — handwrite the JSON. The shape is
    // small and stable, and avoiding the dep keeps the crate slim.
    let mut out = String::new();
    out.push_str("{\n  \"files\": [\n");
    for (i, path) in args.paths.iter().enumerate() {
        let data = fs::read(path)?;
        let analysis = analyze(&data)?;
        if i > 0 {
            out.push_str(",\n");
        }
        let entry = json_entry(path, &analysis, any_unknown);
        out.push_str(&entry);
    }
    out.push_str("\n  ]\n}");
    println!("{out}");
    Ok(())
}

fn json_entry(path: &std::path::Path, analysis: &Analysis, any_unknown: &mut bool) -> String {
    let mut s = String::new();
    s.push_str("    {\n");
    s.push_str(&format!(
        "      \"path\": {},\n",
        json_string(&path.display().to_string())
    ));
    match analysis {
        Analysis::Obj(o) => {
            s.push_str("      \"kind\": \"obj\",\n");
            s.push_str(&format!("      \"tier\": \"{}\",\n", o.tier().slug()));
            s.push_str(&format!(
                "      \"name\": {},\n",
                json_string_opt(o.name.as_deref())
            ));
            s.push_str(&format!(
                "      \"translator\": {},\n",
                yn_bool(o.has_bcc_translator())
            ));
            s.push_str(&format!(
                "      \"ea_marker\": {},\n",
                yn_bool(o.ea_marker.is_some())
            ));
            s.push_str(&format!(
                "      \"e8_trailer\": {},\n",
                yn_bool(o.e8_trailer.is_some())
            ));
            s.push_str(&format!("      \"a1_marker\": {},\n", yn_bool(o.a1_marker_empty)));
            s.push_str(&format!(
                "      \"bcc_lnames\": {},\n",
                yn_bool(o.matches_bcc_lnames())
            ));
            s.push_str(&format!(
                "      \"bcc_prologue\": {}\n",
                yn_bool(o.has_bcc_prologue())
            ));
            if matches!(o.tier(), FingerprintTier::Unknown) {
                *any_unknown = true;
            }
        }
        Analysis::Lib(l) => {
            s.push_str("      \"kind\": \"lib\",\n");
            s.push_str(&format!("      \"page_size\": {},\n", l.page_size));
            s.push_str(&format!("      \"member_count\": {},\n", l.members.len()));
            s.push_str("      \"tier_counts\": {\n");
            let counts: Vec<String> = l
                .tier_counts
                .iter()
                .map(|(slug, n)| format!("        {}: {n}", json_string(slug)))
                .collect();
            s.push_str(&counts.join(",\n"));
            s.push_str("\n      }\n");
        }
        Analysis::Unknown { first_byte } => {
            s.push_str("      \"kind\": \"unknown\",\n");
            s.push_str(&format!("      \"first_byte\": {first_byte}\n"));
            *any_unknown = true;
        }
    }
    s.push_str("    }");
    s
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_string_opt(s: Option<&str>) -> String {
    match s {
        Some(s) => json_string(s),
        None => "null".to_string(),
    }
}

fn yn_bool(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}
