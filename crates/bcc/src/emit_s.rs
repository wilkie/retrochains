//! Emit the `.ASM` text that `BCC -S` produces. See
//! `specs/bcc/ASM_OUTPUT.md` for the format. The bytes in this file are
//! the file-level scaffolding (macro preamble, segment scaffold, tail);
//! everything that varies per-function is driven by [`crate::codegen`].

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::codegen;
use crate::dos_time;
use crate::lex::Lexer;
use crate::parse::Parser;

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("source {0}: {1}")]
    SourceRead(PathBuf, std::io::Error),
    #[error("lex: {0}")]
    Lex(#[from] crate::lex::LexError),
    #[error("parse: {0}")]
    Parse(#[from] crate::parse::ParseError),
}

/// Compile one `.C` source to `.ASM` next to it in the current directory.
///
/// # Errors
/// Returns [`EmitError`] on I/O failures, lex errors, or parse errors.
pub fn emit_dash_s(source_path: &Path) -> Result<PathBuf, EmitError> {
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
    let output_path = PathBuf::from(format!("{}.ASM", basename.to_ascii_uppercase()));

    let bytes = build_asm(&source, &lowered, mtime)?;
    fs::write(&output_path, bytes)?;
    Ok(output_path)
}

/// Produce the ASM file bytes from a source string and the associated
/// metadata. Pure for testability.
///
/// # Errors
/// Returns [`EmitError`] on lex or parse failures.
pub fn build_asm(
    source: &str,
    source_filename_lower: &str,
    mtime: SystemTime,
) -> Result<Vec<u8>, EmitError> {
    let tokens = Lexer::new(source).tokenize()?;
    let unit = Parser::new(tokens).parse_unit()?;

    let mut out = Vec::with_capacity(1024);
    write_macro_preamble(&mut out);
    write_debug_header(&mut out, source_filename_lower, mtime);
    write_segment_scaffold(&mut out);

    for function in &unit.functions {
        codegen::emit_function(&mut out, source, function);
    }

    write_tail(&mut out, &unit);
    out.push(0x1A); // DOS EOF marker
    Ok(out)
}

fn write_macro_preamble(out: &mut Vec<u8>) {
    // Byte-for-byte from the captured fixture. See specs/bcc/ASM_OUTPUT.md.
    const PREAMBLE: &[u8] = b"\
\tifndef\t??version\r\n\
?debug\tmacro\r\n\
\tendm\r\n\
publicdll macro\tname\r\n\
\tpublic\tname\r\n\
\tendm\r\n\
$comm\tmacro\tname,dist,size,count\r\n\
\tcomm\tdist name:BYTE:count*size\r\n\
\tendm\r\n\
\telse\r\n\
$comm\tmacro\tname,dist,size,count\r\n\
\tcomm\tdist name[size]:BYTE:count\r\n\
\tendm\r\n\
\tendif\r\n";
    out.extend_from_slice(PREAMBLE);
}

fn write_debug_header(out: &mut Vec<u8>, filename_lower: &str, mtime: SystemTime) {
    // ?debug S "<filename>"
    let _ = write!(out, "\t?debug\tS \"{filename_lower}\"\r\n");

    // ?debug C <hex-bytes>
    //   layout: E9 <packed-mtime u32 LE> <name-len u8> <name bytes>
    let packed = dos_time::pack(mtime);
    let mut payload: Vec<u8> = Vec::with_capacity(6 + filename_lower.len());
    payload.push(0xE9);
    payload.extend_from_slice(&packed.to_le_bytes());
    let name_len = u8::try_from(filename_lower.len()).unwrap_or(0);
    payload.push(name_len);
    payload.extend_from_slice(filename_lower.as_bytes());
    out.extend_from_slice(b"\t?debug\tC ");
    for b in payload {
        let _ = write!(out, "{b:02X}");
    }
    out.extend_from_slice(b"\r\n");
}

fn write_segment_scaffold(out: &mut Vec<u8>) {
    const SCAFFOLD: &[u8] = b"\
_TEXT\tsegment byte public 'CODE'\r\n\
_TEXT\tends\r\n\
DGROUP\tgroup\t_DATA,_BSS\r\n\
\tassume\tcs:_TEXT,ds:DGROUP\r\n\
_DATA\tsegment word public 'DATA'\r\n\
d@\tlabel\tbyte\r\n\
d@w\tlabel\tword\r\n\
_DATA\tends\r\n\
_BSS\tsegment word public 'BSS'\r\n\
b@\tlabel\tbyte\r\n\
b@w\tlabel\tword\r\n\
_BSS\tends\r\n";
    out.extend_from_slice(SCAFFOLD);
}

fn write_tail(out: &mut Vec<u8>, unit: &crate::ast::Unit) {
    out.extend_from_slice(b"_DATA\tsegment word public 'DATA'\r\n");
    out.extend_from_slice(b"s@\tlabel\tbyte\r\n");
    out.extend_from_slice(b"_DATA\tends\r\n");
    out.extend_from_slice(b"_TEXT\tsegment byte public 'CODE'\r\n");
    out.extend_from_slice(b"_TEXT\tends\r\n");
    for f in &unit.functions {
        let _ = write!(out, "\tpublic\t{}\r\n", codegen::function_symbol(&f.name));
    }
    out.extend_from_slice(b"\tend\r\n");
}
