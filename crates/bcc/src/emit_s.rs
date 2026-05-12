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

    // Initialized globals live in a `_DATA` block at the top of the
    // file, between the empty segment scaffold and the function-code
    // `_TEXT segment`. Fixtures 084, 086, 087.
    let has_init_globals = unit.globals.iter().any(|g| g.init.is_some());
    if has_init_globals {
        write_init_globals(&mut out, &unit);
    }

    // _TEXT opens once for the whole translation unit; every function
    // lives inside.
    out.extend_from_slice(b"_TEXT\tsegment byte public 'CODE'\r\n");
    let signatures = codegen::Signatures::from_unit(&unit);
    let globals = codegen::GlobalTable::from_unit(&unit);
    let mut strings = codegen::StringPool::default();
    for (idx, function) in unit.functions.iter().enumerate() {
        let func_idx = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        codegen::emit_function(
            &mut out,
            source,
            function,
            func_idx,
            &signatures,
            &globals,
            &mut strings,
        );
    }

    // `?debug C E9` placement and `_TEXT ends` ordering depend on
    // whether there are uninitialized globals. With none, the debug
    // record sits inside _TEXT before its close (the original
    // shape). With some, _TEXT closes first, _BSS opens, the globals
    // are emitted, and then `?debug C E9` lands just before `_BSS
    // ends` (fixtures 083, 085, 087).
    let has_bss_globals = unit.globals.iter().any(|g| g.init.is_none());
    if has_bss_globals {
        out.extend_from_slice(b"_TEXT\tends\r\n");
        write_bss_globals_with_debug(&mut out, &unit);
    } else {
        out.extend_from_slice(b"\t?debug\tC E9\r\n");
        out.extend_from_slice(b"_TEXT\tends\r\n");
    }

    write_tail(&mut out, &unit, &strings);
    out.push(0x1A); // DOS EOF marker
    Ok(out)
}

/// Emit initialized globals in `_DATA` at the top of the file.
/// Each global gets a `_<name> label <word|byte>` followed by `db`
/// bytes for its initialized value (little-endian).
fn write_init_globals(out: &mut Vec<u8>, unit: &crate::ast::Unit) {
    out.extend_from_slice(b"_DATA\tsegment word public 'DATA'\r\n");
    for g in &unit.globals {
        let Some(init) = &g.init else { continue };
        emit_global_decl(out, &g.name, &g.ty);
        emit_global_init(out, &g.ty, init);
    }
    out.extend_from_slice(b"_DATA\tends\r\n");
}

/// Emit uninitialized globals in `_BSS` at the bottom of the file,
/// with the function-end `?debug C E9` record placed before
/// `_BSS ends` (fixture 087).
fn write_bss_globals_with_debug(out: &mut Vec<u8>, unit: &crate::ast::Unit) {
    out.extend_from_slice(b"_BSS\tsegment word public 'BSS'\r\n");
    for g in &unit.globals {
        if g.init.is_some() {
            continue;
        }
        emit_global_decl(out, &g.name, &g.ty);
        let size = g.ty.size_bytes();
        let _ = write!(out, "\tdb\t{size} dup (?)\r\n");
    }
    out.extend_from_slice(b"\t?debug\tC E9\r\n");
    out.extend_from_slice(b"_BSS\tends\r\n");
}

/// `_<name> label <word|byte>` — the per-global anchor that
/// precedes the actual storage `db`s.
fn emit_global_decl(out: &mut Vec<u8>, name: &str, ty: &crate::ast::Type) {
    let width = if ty.size_bytes() >= 2 { "word" } else { "byte" };
    let _ = write!(out, "_{name}\tlabel\t{width}\r\n");
}

/// Emit the `db` byte run for an initialized global's value. Only
/// constant initializers are supported today — non-constant
/// initializers at file scope aren't legal C anyway.
fn emit_global_init(out: &mut Vec<u8>, ty: &crate::ast::Type, init: &crate::ast::Expr) {
    let v = codegen::fold_const_global(init).unwrap_or_else(|| {
        panic!("non-constant initializer at file scope (no fixture yet supports this)")
    });
    let size = ty.size_bytes();
    if size == 1 {
        let _ = write!(out, "\tdb\t{}\r\n", v & 0xFF);
    } else {
        // int (and pointer) globals: little-endian byte pair, same
        // shape BCC uses for the linear-search switch value table.
        let _ = write!(out, "\tdb\t{}\r\n", v & 0xFF);
        let _ = write!(out, "\tdb\t{}\r\n", (v >> 8) & 0xFF);
    }
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

fn write_tail(out: &mut Vec<u8>, unit: &crate::ast::Unit, strings: &codegen::StringPool) {
    out.extend_from_slice(b"_DATA\tsegment word public 'DATA'\r\n");
    out.extend_from_slice(b"s@\tlabel\tbyte\r\n");
    // String literals materialize here. Each entry becomes a
    // `db '<chars>' / db 0` pair, with the NUL terminator written
    // explicitly so escapes inside the literal don't have to be
    // re-quoted. Fixtures 088, 089.
    for entry in strings.entries() {
        emit_string_literal_db(out, entry);
        out.extend_from_slice(b"\tdb\t0\r\n");
    }
    out.extend_from_slice(b"_DATA\tends\r\n");
    out.extend_from_slice(b"_TEXT\tsegment byte public 'CODE'\r\n");
    out.extend_from_slice(b"_TEXT\tends\r\n");
    // Public symbols are emitted in **reverse alphabetical** order.
    // Earlier fixtures (009, 010, 087) happened to land in alpha
    // order in source too, which led us to assume "reverse source
    // order" — but fixture 095 (`int sum(...); int main(...)`,
    // public list `sum, main`) disambiguates: source order is sum,
    // main, alphabetical is main, sum, and the emitted order is
    // sum, main — the reverse-alpha walk. The most likely
    // explanation is that BCC keeps its symbol table sorted and
    // walks it in reverse at TU end.
    let mut names: Vec<String> = Vec::new();
    for f in &unit.functions {
        names.push(codegen::function_symbol(&f.name));
    }
    for g in &unit.globals {
        names.push(format!("_{}", g.name));
    }
    names.sort();
    for name in names.iter().rev() {
        let _ = write!(out, "\tpublic\t{name}\r\n");
    }
    out.extend_from_slice(b"\tend\r\n");
}

/// Render a string literal's bytes as a single `db '...'` line,
/// matching the way TASM accepts string literals. Quote handling
/// (`'` inside the string) and non-printables would need attention
/// for fancier fixtures; today our captures only have plain ASCII.
fn emit_string_literal_db(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(b"\tdb\t'");
    for &b in bytes {
        // Escape embedded `'` by closing the string, emitting a
        // separate byte, and reopening. This is one of TASM's
        // accepted forms; revisit when a fixture has an embedded quote.
        if b == b'\'' {
            out.extend_from_slice(b"',39,'");
        } else {
            out.push(b);
        }
    }
    out.extend_from_slice(b"'\r\n");
}
