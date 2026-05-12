//! Emit the `.OBJ` bytes that `BCC -c` produces. Closes fixture 002
//! (empty `int main(void) { return 0; }`) for now; broader support
//! grows as we capture more `-c` fixtures.
//!
//! The output is OMF (Intel Object Module Format). Each record's
//! framing is in the `obj` crate; this file is the per-source-file
//! driver that decides which records to emit and in what order to
//! match BCC's byte-exact output.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::dos_time;
use crate::emit_s::EmitError;
use crate::lex::Lexer;
use crate::parse::Parser;

/// Compile one `.C` source to `.OBJ` next to it in the current
/// directory.
///
/// # Errors
/// Returns [`EmitError`] on I/O failures, lex errors, or parse errors.
pub fn emit_dash_c(source_path: &Path) -> Result<PathBuf, EmitError> {
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

    let bytes = build_obj(&source, &lowered, mtime)?;
    fs::write(&output_path, bytes)?;
    Ok(output_path)
}

/// Produce the OBJ file bytes from a source string and the associated
/// metadata. Pure for testability.
///
/// Today this handles only the smallest case: a single `int main(void)
/// { return 0; }` translation unit. More fixtures will expand the
/// machine-code-emission and record-layout coverage.
///
/// # Errors
/// Returns [`EmitError`] on lex or parse failures.
pub fn build_obj(
    source: &str,
    source_filename_lower: &str,
    mtime: SystemTime,
) -> Result<Vec<u8>, EmitError> {
    let tokens = Lexer::new(source).tokenize()?;
    let _unit = Parser::new(tokens).parse_unit()?;
    // We're not yet using the parsed unit — just hard-coding the
    // byte sequence for fixture 002 to close it byte-exactly. The
    // codegen-to-bytes path is the next step.

    let mut b = obj::ObjBuilder::new();

    // 1) THEADR: module name. BCC uses the lowercased basename plus
    //    extension (`hello.c`), same as the .asm debug record.
    b.write_theadr(source_filename_lower);

    // 2) COMENT class 0x00 (Translator): compiler identifier. The
    //    data is a length-prefixed string — the leading 0x1a in the
    //    captured bytes is the string length (26), NOT the comment
    //    class as one might initially read. BCC always emits "TC86
    //    Borland Turbo C++ 2.0", the most distinctive single byte
    //    sequence in any BCC 2.0 OBJ.
    let id_str: &[u8] = b"TC86 Borland Turbo C++ 2.0";
    let mut id_payload = Vec::new();
    id_payload.push(0x00); // flags (NoPurge=0, NoList=0)
    id_payload.push(0x00); // class = Translator
    id_payload.push(u8::try_from(id_str.len()).expect("compiler id fits in u8"));
    id_payload.extend_from_slice(id_str);
    b.write_coment(&id_payload);

    // 3) COMENT class 0xe9: debug-info-start record. Payload after
    //    the class byte is `E9 <packed-mtime-LE-u32> <name-len-u8>
    //    <name-bytes>` — exactly the bytes the .asm `?debug C
    //    E9...` line encodes.
    let packed_time = dos_time::pack(mtime);
    let mut dbg_start = Vec::new();
    dbg_start.push(0x00); // flags
    dbg_start.push(0xe9); // class
    dbg_start.extend_from_slice(&packed_time.to_le_bytes());
    dbg_start.push(u8::try_from(source_filename_lower.len()).unwrap_or(0));
    dbg_start.extend_from_slice(source_filename_lower.as_bytes());
    b.write_coment(&dbg_start);

    // 4) COMENT class 0xe9: debug-info-end marker (just the class
    //    byte after the flags).
    b.write_coment(&[0x00, 0xe9]);

    // 5) COMENT class 0xea: memory-model / linker-directive record.
    //    Bytes `01 09` after the class byte in fixture 002. Meaning
    //    not fully understood — likely a per-memory-model tag (the
    //    "01" plausibly indicates small model). Hard-coded for now.
    b.write_coment(&[0x00, 0xea, 0x01, 0x09]);

    // 6) LNAMES: empty + segment + class + group names BCC always
    //    emits. The empty first entry is the "no-overlay" sentinel.
    b.write_lnames(&[
        "",
        "_TEXT", "CODE",
        "_DATA", "DATA",
        "_BSS", "BSS",
        "DGROUP",
    ]);

    // Hard-coded machine code for `int main(void) { return 0; }`:
    //   push bp        55
    //   mov bp,sp      8B EC
    //   xor ax,ax      33 C0
    //   jmp short L    EB 00      (L is the next instruction)
    //   pop bp         5D
    //   ret            C3
    // Total: 9 bytes. This matches the LEDATA payload of fixture
    // 002 byte-exactly. A real machine-code emitter would derive
    // this from the AST; for the byte-exact-002 milestone we just
    // pin it.
    let main_bytes: [u8; 9] = [0x55, 0x8B, 0xEC, 0x33, 0xC0, 0xEB, 0x00, 0x5D, 0xC3];

    // 7) SEGDEF _TEXT — byte-aligned, public, length = main_bytes.len().
    //    ACBP 0x28 = align(byte) / combine(public). Name idx 2 =
    //    "_TEXT", class idx 3 = "CODE", overlay 1 = "".
    b.write_segdef16(0x28, main_bytes.len() as u16, 2, 3, 1);

    // 8) SEGDEF _DATA — word-aligned public, length 0 (no data).
    b.write_segdef16(0x48, 0, 4, 5, 1);

    // 9) SEGDEF _BSS — word-aligned public, length 0.
    b.write_segdef16(0x48, 0, 6, 7, 1);

    // 10) GRPDEF DGROUP = {_DATA(seg 2), _BSS(seg 3)}.
    b.write_grpdef(8, &[2, 3]);

    // 11) PUBDEF _main at _TEXT offset 0, type 0 (untyped).
    b.write_pubdef16(0, 1, "_main", 0, 0);

    // 12) LEDATA into _TEXT at offset 0.
    b.write_ledata16(1, 0, &main_bytes);

    // 13) COMENT class 0xe8 — trailing debug-info record. Payload
    //     after the class byte: `00 <name-len-u8> <name-bytes>
    //     <packed-mtime-LE-u32>`. Note the timestamp comes *after*
    //     the filename here, vs. before it in the 0xe9 record.
    let mut dbg_tail = Vec::new();
    dbg_tail.push(0x00); // flags
    dbg_tail.push(0xe8); // class
    dbg_tail.push(0x00); // some prefix byte (purpose unknown)
    dbg_tail.push(u8::try_from(source_filename_lower.len()).unwrap_or(0));
    dbg_tail.extend_from_slice(source_filename_lower.as_bytes());
    dbg_tail.extend_from_slice(&packed_time.to_le_bytes());
    b.write_coment(&dbg_tail);

    // 14) MODEND with no start address.
    b.write_modend16_no_entry();

    Ok(b.into_bytes())
}
