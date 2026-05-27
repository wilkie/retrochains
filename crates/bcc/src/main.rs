use std::process::ExitCode;

use bcc::{CompileMode, emit_dash_c, emit_dash_s, parse_args};

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("bcc: {e}");
            ExitCode::from(1)
        }
    }
}

fn try_main() -> Result<(), Box<dyn std::error::Error>> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let parsed = parse_args(&raw)?;
    match parsed.mode {
        CompileMode::Assembly => {
            for src in &parsed.sources {
                emit_dash_s(src, parsed.merge_strings, &parsed.defines, parsed.unsigned_chars)?;
            }
            Ok(())
        }
        CompileMode::Object => {
            for src in &parsed.sources {
                emit_dash_c(src, parsed.merge_strings, &parsed.defines, parsed.unsigned_chars)?;
            }
            Ok(())
        }
    }
}
