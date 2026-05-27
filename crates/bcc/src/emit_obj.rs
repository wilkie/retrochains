//! Emit the `.OBJ` bytes that `BCC -c` produces.
//!
//! We don't have a parallel codegen-to-bytes path. Instead, the `-c`
//! flow is: generate the `.ASM` text the same way `-S` does, then
//! assemble it through the in-house `tasm` crate (`specs/formats/ASM.md`
//! + `specs/formats/OMF.md`). This means every byte-exactness improvement
//! to `-S` automatically lifts `-c` too, and the `-S` → `-c` pipeline
//! mirrors what Borland's toolchain actually does (BCC → TASM).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::emit_s::{build_asm, EmitError};

/// Compile one `.C` source to `.OBJ` next to it in the current directory.
///
/// # Errors
/// Returns [`EmitError`] on I/O failures, lex errors, parse errors, or
/// assembler errors.
pub fn emit_dash_c(
    source_path: &Path,
    merge_strings: bool,
    defines: &[(String, String)],
    unsigned_chars: bool,
    optimize: bool,
    target_186: bool,
    stack_check: bool,
) -> Result<PathBuf, EmitError> {
    let source = fs::read_to_string(source_path)
        .map_err(|e| EmitError::SourceRead(source_path.to_owned(), e))?;
    let mtime = fs::metadata(source_path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let basename = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("OUT")
        .to_string();
    let lowered = source_path
        .file_name()
        .and_then(|s| s.to_str())
        .map_or_else(|| "out.c".to_owned(), str::to_ascii_lowercase);
    let output_path = PathBuf::from(format!("{}.OBJ", basename.to_ascii_uppercase()));

    let bytes = build_obj(&source, &lowered, mtime, merge_strings, defines, unsigned_chars, optimize, target_186, stack_check)?;
    fs::write(&output_path, bytes)?;
    Ok(output_path)
}

/// Produce the OBJ bytes for a source file. Pure for testability.
///
/// Pipeline:
///   1. Compile the source to ASM text (same path as `bcc -S`).
///   2. Assemble that text via [`tasm::assemble`] to produce OMF bytes.
///
/// # Errors
/// Returns [`EmitError`] on any lex/parse/assemble failure.
pub fn build_obj(
    source: &str,
    source_filename_lower: &str,
    mtime: SystemTime,
    merge_strings: bool,
    defines: &[(String, String)],
    unsigned_chars: bool,
    optimize: bool,
    target_186: bool,
    stack_check: bool,
) -> Result<Vec<u8>, EmitError> {
    let asm_bytes = build_asm(source, source_filename_lower, mtime, merge_strings, defines, unsigned_chars, optimize, target_186, stack_check)?;
    // build_asm produces UTF-8 ASCII bytes (BCC's text is pure ASCII
    // plus the trailing 0x1A EOF byte). Convert to a &str for tasm.
    let asm_text =
        std::str::from_utf8(&asm_bytes).map_err(|e| EmitError::AsmNotUtf8(e.to_string()))?;
    tasm::assemble(asm_text).map_err(EmitError::Assemble)
}
