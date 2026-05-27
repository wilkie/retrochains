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
use crate::preprocess;

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("source {0}: {1}")]
    SourceRead(PathBuf, std::io::Error),
    #[error("preprocess: {0}")]
    Preprocess(#[from] preprocess::PreprocessError),
    #[error("lex: {0}")]
    Lex(#[from] crate::lex::LexError),
    #[error("parse: {0}")]
    Parse(#[from] crate::parse::ParseError),
    #[error("internal: ASM output is not valid UTF-8: {0}")]
    AsmNotUtf8(String),
    #[error("assemble: {0}")]
    Assemble(tasm::AsmError),
}

/// Compile one `.C` source to `.ASM` next to it in the current directory.
///
/// # Errors
/// Returns [`EmitError`] on I/O failures, lex errors, or parse errors.
pub fn emit_dash_s(
    source_path: &Path,
    merge_strings: bool,
    defines: &[(String, String)],
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
    let output_path = PathBuf::from(format!("{}.ASM", basename.to_ascii_uppercase()));

    let bytes = build_asm(&source, &lowered, mtime, merge_strings, defines)?;
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
    merge_strings: bool,
    defines: &[(String, String)],
) -> Result<Vec<u8>, EmitError> {
    // C preprocessor pass: resolve `#define`/`#ifdef`/`#if` and
    // expand object/function-like macros. Stripped directive lines
    // are replaced with blank lines so byte/line numbering in the
    // output matches the original source — the codegen still uses
    // the ORIGINAL source for `;`-comment line emission so macro
    // names show through in comments even though the code uses the
    // expanded form.
    let preprocessed = preprocess::preprocess_with_defines(source, defines)?;
    let tokens = Lexer::new(&preprocessed).tokenize()?;
    let unit = Parser::new(tokens).parse_unit()?;

    let mut out = Vec::with_capacity(1024);
    write_macro_preamble(&mut out);
    write_debug_header(&mut out, source_filename_lower, mtime);
    write_segment_scaffold(&mut out);

    // The string pool is owned here so both initialized-global emission
    // (file scope `char *p = "lit"` — fixture 192) and per-function code
    // emission can intern into the same `s@`-relative table.
    let mut strings = codegen::StringPool::default();
    strings.merge_strings = merge_strings;

    // Initialized globals live in a `_DATA` block at the top of the
    // file, between the empty segment scaffold and the function-code
    // `_TEXT segment`. Fixtures 084, 086, 087. Externs declare storage
    // elsewhere, so they don't contribute to _DATA or _BSS.
    let has_init_globals = unit
        .globals
        .iter()
        .any(|g| !g.is_extern && g.init.is_some());
    if has_init_globals {
        write_init_globals(&mut out, &unit, &mut strings);
    }

    // _TEXT opens once for the whole translation unit; every function
    // *definition* lives inside (prototypes don't produce any asm).
    out.extend_from_slice(b"_TEXT\tsegment byte public 'CODE'\r\n");
    let signatures = codegen::Signatures::from_unit(&unit);
    let globals = codegen::GlobalTable::from_unit(&unit);
    // Codegen-injected runtime-helper externs (e.g. `N_LXLSH@` for
    // long left-shift, fixture 228). Accumulated across all
    // functions, then merged into the publics-ordering bucket
    // emission in `write_tail`.
    let mut helpers: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Function-index assignment skips prototypes — they don't get a
    // `@N@…` label scope of their own.
    let mut func_idx = 0u32;
    for function in &unit.functions {
        if function.body.is_none() {
            continue;
        }
        func_idx += 1;
        codegen::emit_function(
            &mut out,
            source,
            function,
            func_idx,
            &signatures,
            &globals,
            &mut strings,
            &mut helpers,
        );
    }

    // `?debug C E9` placement and `_TEXT ends` ordering depend on
    // whether there are uninitialized globals. With none, the debug
    // record sits inside _TEXT before its close (the original
    // shape). With some, _TEXT closes first, _BSS opens, the globals
    // are emitted, and then `?debug C E9` lands just before `_BSS
    // ends` (fixtures 083, 085, 087).
    let has_bss_globals = unit
        .globals
        .iter()
        .any(|g| !g.is_extern && g.init.is_none());
    if has_bss_globals {
        out.extend_from_slice(b"_TEXT\tends\r\n");
        write_bss_globals_with_debug(&mut out, &unit);
    } else {
        out.extend_from_slice(b"\t?debug\tC E9\r\n");
        out.extend_from_slice(b"_TEXT\tends\r\n");
    }

    write_tail(&mut out, &unit, &strings, &helpers);
    out.push(0x1A); // DOS EOF marker
    Ok(out)
}

/// Emit initialized globals in `_DATA` at the top of the file.
/// Each global gets a `_<name> label <word|byte>` followed by `db`
/// bytes for its initialized value (little-endian).
fn write_init_globals(
    out: &mut Vec<u8>,
    unit: &crate::ast::Unit,
    strings: &mut codegen::StringPool,
) {
    out.extend_from_slice(b"_DATA\tsegment word public 'DATA'\r\n");
    for g in &unit.globals {
        if g.is_extern {
            continue;
        }
        let Some(init) = &g.init else { continue };
        emit_global_decl(out, &g.name, &g.ty);
        emit_global_init(out, &g.ty, init, strings);
    }
    out.extend_from_slice(b"_DATA\tends\r\n");
}

/// Emit uninitialized globals in `_BSS` at the bottom of the file,
/// with the function-end `?debug C E9` record placed before
/// `_BSS ends` (fixture 087).
fn write_bss_globals_with_debug(out: &mut Vec<u8>, unit: &crate::ast::Unit) {
    out.extend_from_slice(b"_BSS\tsegment word public 'BSS'\r\n");
    // BCC's _BSS layout: short-named globals (`_<n>` with name
    // length < 3) first in alphabetical order, then long-named
    // globals in alphabetical order. The same length-bucket
    // discriminant the publics ordering uses; this is the *reverse*
    // of the publics emission order, filtered to BSS members. A
    // 1-byte filler `db 1 dup (?)` is inserted when the running
    // offset is odd before a word-aligned global. Pinned by
    // fixtures 181 (all 2-char names → alpha order `a, c, pad, x`),
    // 462/234 (all 2-char names → alpha), and 465
    // (`buf` (4) + `g` (2) → short bucket emits `g` first, then
    // long bucket emits `buf` — no padding needed).
    let mut short_bss: Vec<&crate::ast::Global> = Vec::new();
    let mut long_bss: Vec<&crate::ast::Global> = Vec::new();
    for g in unit.globals.iter().filter(|g| !g.is_extern && g.init.is_none()) {
        let sym_len = g.name.len() + 1; // `_<name>` mangling
        if sym_len < 3 {
            short_bss.push(g);
        } else {
            long_bss.push(g);
        }
    }
    short_bss.sort_by(|a, b| a.name.cmp(&b.name));
    // Long bucket: source order. Fixture 3059 (`struct M src;
    // struct M dst;` → src, dst, contradicting an alpha sort).
    // 1401 (`struct S s1; struct S s2;`) passes alpha trivially
    // because source order matches alpha there.
    let bss: Vec<&crate::ast::Global> =
        short_bss.into_iter().chain(long_bss.into_iter()).collect();
    let mut offset: u16 = 0;
    for g in bss {
        let align = g.ty.alignment();
        if align == 2 && offset % 2 == 1 {
            out.extend_from_slice(b"\tdb\t1 dup (?)\r\n");
            offset += 1;
        }
        emit_global_decl(out, &g.name, &g.ty);
        let size = g.ty.size_bytes();
        let _ = write!(out, "\tdb\t{size} dup (?)\r\n");
        offset += size;
    }
    out.extend_from_slice(b"\t?debug\tC E9\r\n");
    out.extend_from_slice(b"_BSS\tends\r\n");
}

/// `_<name> label <word|byte>` — the per-global anchor that
/// precedes the actual storage `db`s. For arrays the width is the
/// element type's width (a char array gets `label byte` regardless
/// of total size, fixture 191).
fn emit_global_decl(out: &mut Vec<u8>, name: &str, ty: &crate::ast::Type) {
    use crate::ast::Type;
    // For an array type, walk to the innermost leaf to pick the
    // width — `char data[2][3]` (Array(Array(Char,3),2)) has leaf
    // `char` (1 byte) so width is `byte`, even though the outer
    // element is an array of size 3. Fixture 2985.
    fn leaf_size(t: &Type) -> u16 {
        match t {
            Type::Array { elem, .. } => leaf_size(elem),
            _ => t.size_bytes(),
        }
    }
    fn leaf_type(t: &Type) -> &Type {
        match t {
            Type::Array { elem, .. } => leaf_type(elem),
            _ => t,
        }
    }
    /// True if the struct (or array-of-struct) contains any bitfield.
    /// Bitfield-only structs that pack into 1 byte still emit `_b
    /// label word` — the label tracks the declared int width, not
    /// the actual byte count. Fixtures 3209, 3322.
    fn has_bitfield(t: &Type) -> bool {
        match t {
            Type::Struct { fields, .. } => {
                fields.iter().any(|f| f.bitfield.is_some())
                    || fields.iter().any(|f| has_bitfield(&f.ty))
            }
            Type::Array { elem, .. } => has_bitfield(elem),
            _ => false,
        }
    }
    // Float/double leaves use width keywords matching their FPU
    // operand prefix: `dword` for single, `qword` for double.
    // Fixture 1680 (`double g = 3.14;` → `_g label qword`).
    let width = match leaf_type(ty) {
        Type::Float => "dword",
        Type::Double => "qword",
        _ if has_bitfield(ty) => "word",
        _ if leaf_size(ty) >= 2 => "word",
        _ => "byte",
    };
    let _ = write!(out, "_{name}\tlabel\t{width}\r\n");
}

/// Emit the `db` byte run for an initialized global's value. Only
/// constant initializers are supported today — non-constant
/// initializers at file scope aren't legal C anyway.
fn emit_global_init(
    out: &mut Vec<u8>,
    ty: &crate::ast::Type,
    init: &crate::ast::Expr,
    strings: &mut codegen::StringPool,
) {
    use crate::ast::{ExprKind, Type};
    // `char s[] = "hi"` (fixture 191) — one `db <byte>` per char plus
    // a trailing `db 0` for the NUL. Parser has already widened the
    // array length to bytes.len()+1. When the declared length exceeds
    // the string size (fixture 498, `char msg[16] = "hello"`), pad the
    // remainder with `db 0` lines out to the full declared length.
    if let (ExprKind::StringLit(bytes), Type::Array { elem, len }) = (&init.kind, ty) {
        if (*elem).is_char_like() {
            // `char s[3] = "abc"` (fixture 2096): declared length
            // matches the string exactly — no room for NUL. C
            // permits this; BCC emits just the 3 bytes with no
            // trailing zero. `char s[] = "abc"` widens len to 4
            // (3 + NUL) at parse time and falls in the room-for-
            // NUL branch.
            for b in bytes {
                let _ = write!(out, "\tdb\t{b}\r\n");
            }
            let written = bytes.len() as u32;
            if (*len as u32) > written {
                let _ = write!(out, "\tdb\t0\r\n");
                let written = written + 1;
                for _ in written..*len {
                    let _ = write!(out, "\tdb\t0\r\n");
                }
            }
            return;
        }
    }
    // `char *p = "lit"` (fixture 192) — pointer global gets a 2-byte
    // slot in _DATA initialized to `DGROUP:s@[+N]`. The literal itself
    // is interned into the same pool used by function-scope literals
    // and emitted later in `write_tail`.
    if let (ExprKind::StringLit(bytes), Type::Pointer(target)) = (&init.kind, ty) {
        if (*target).is_char_like() {
            let offset = strings.intern(bytes);
            if offset == 0 {
                out.extend_from_slice(b"\tdw\tDGROUP:s@\r\n");
            } else {
                let _ = write!(out, "\tdw\tDGROUP:s@+{offset}\r\n");
            }
            return;
        }
    }
    // `T *p = &g;` (fixture 193) — pointer slot initialized to the
    // DGROUP-relative address of another global. Same FIXUPP shape as
    // the string-pool path, but the target is `_<name>` rather than
    // `s@`.
    if let (ExprKind::AddressOf(target_name), Type::Pointer(_)) = (&init.kind, ty) {
        let _ = write!(out, "\tdw\tDGROUP:_{target_name}\r\n");
        return;
    }
    // `T *p = &arr[K];` (fixture 198) — same shape but with a
    // constant byte offset baked in: `dw DGROUP:_arr+<offset>`.
    if let (ExprKind::AddressOfArrayElem { array, byte_offset }, Type::Pointer(_)) =
        (&init.kind, ty)
    {
        if *byte_offset == 0 {
            let _ = write!(out, "\tdw\tDGROUP:_{array}\r\n");
        } else {
            let _ = write!(out, "\tdw\tDGROUP:_{array}+{byte_offset}\r\n");
        }
        return;
    }
    // Aggregate initializer list — emit each item against the array's
    // element type. Fixture 189 (`int a[3] = {1, 2, 3}`) drops six
    // `db` lines, two per element. Excess initializers beyond `len`
    // would be an error in C; we don't fixture-test that path.
    if let ExprKind::InitList { items } = &init.kind {
        match ty {
            Type::Array { elem, len } => {
                for item in items {
                    emit_global_init(out, elem, item, strings);
                }
                // Partial initializer (`int a[5] = {1, 2}` — fixture
                // 502). Pad the remaining slots with zero bytes,
                // emitted as `db 0` lines matching what BCC does.
                let written = items.len() as u32;
                if *len > written {
                    let pad_bytes =
                        u32::from(elem.size_bytes()) * (*len - written);
                    for _ in 0..pad_bytes {
                        let _ = write!(out, "\tdb\t0\r\n");
                    }
                }
            }
            Type::Struct { fields, .. } => {
                if items.len() > fields.len() {
                    panic!("too many initializers for struct ({} fields)", fields.len());
                }
                // Pair each item with the corresponding field's type
                // in declaration order. Fixture 190 (`struct point g
                // = {3, 7}`). Field-by-field, no padding for word-
                // aligned fields in this fixture; alignment fillers
                // for char-followed-by-int would need an extra fixture.
                for (item, field) in items.iter().zip(fields.iter()) {
                    emit_global_init(out, &field.ty, item, strings);
                }
                // Partial struct initializer: pad uninitialized
                // trailing fields with zero bytes — BCC emits `db 0`
                // for each missing byte rather than truncating the
                // record. Fixture 2098 (`static struct S s = {10,
                // 20}` with 3 int fields → 2 trailing zero bytes).
                for field in fields.iter().skip(items.len()) {
                    for _ in 0..field.ty.size_bytes() {
                        let _ = write!(out, "\tdb\t0\r\n");
                    }
                }
            }
            _ => panic!("initializer list against {:?} not yet supported", ty),
        }
        return;
    }
    emit_scalar_global_bytes(out, ty, init);
}

fn emit_scalar_global_bytes(
    out: &mut Vec<u8>,
    ty: &crate::ast::Type,
    init: &crate::ast::Expr,
) {
    use crate::ast::{ExprKind, Type};
    // Float/double globals use the IEEE 754 bits directly (the
    // FPU has no notion of "integer immediate", so we don't go
    // through `fold_const_global`). A `double = <float-representable>`
    // constant still pools full 8 bytes here — global initializers
    // are linker-resolved and don't get the FPU-promotion trick
    // that on-stack inits use.
    if let (ExprKind::FloatLit(bits), Type::Float) = (&init.kind, ty) {
        for &b in bits.to_le_bytes().iter() {
            let _ = write!(out, "\tdb\t{b}\r\n");
        }
        return;
    }
    if matches!(ty, Type::Double) {
        let bits: u64 = match &init.kind {
            ExprKind::DoubleLit(b) => *b,
            ExprKind::FloatLit(b) => {
                // `double g = 3.0f;` — widen single to double for
                // the in-memory image.
                (f32::from_bits(*b) as f64).to_bits()
            }
            _ => panic!("non-literal double global initializer"),
        };
        for &b in bits.to_le_bytes().iter() {
            let _ = write!(out, "\tdb\t{b}\r\n");
        }
        return;
    }
    // `T *p = <arr> + K;` — array-decay + constant offset. BCC
    // emits `dw DGROUP:_<arr>+(K*stride)` with the linker resolving
    // the symbol address. Fixture 3222.
    if matches!(ty, Type::Pointer(_))
        && let ExprKind::BinOp { op: crate::ast::BinOp::Add, left, right } = &init.kind
        && let ExprKind::Ident(arr_name) = &left.kind
        && let Some(k) = codegen::fold_const_global(right)
    {
        // We need the element size to compute the byte offset.
        // Since emit_global_init doesn't have the globals table,
        // pull the stride from the pointer's pointee type.
        let stride = if let Type::Pointer(inner) = ty {
            u32::from(inner.size_bytes())
        } else {
            unreachable!()
        };
        let offset = k.wrapping_mul(stride);
        if offset == 0 {
            let _ = write!(out, "\tdw\tDGROUP:_{arr_name}\r\n");
        } else {
            let _ = write!(out, "\tdw\tDGROUP:_{arr_name}+{offset}\r\n");
        }
        return;
    }
    // Bare-ident initializer of a near pointer — array decay
    // `T *p = arr;`. Same shape as the constant-offset path but
    // with offset 0. Fixtures 2607 etc.
    if matches!(ty, Type::Pointer(_))
        && let ExprKind::Ident(arr_name) = &init.kind
    {
        let _ = write!(out, "\tdw\tDGROUP:_{arr_name}\r\n");
        return;
    }
    let v = codegen::fold_const_global(init).unwrap_or_else(|| {
        panic!("non-constant initializer at file scope (no fixture yet supports this)")
    });
    let size = ty.size_bytes();
    // Emit `size` bytes little-endian, one `db <byte>` per byte.
    // Covers `char` (1 byte), `int`/pointer (2 bytes), and `long`
    // (4 bytes, fixture 204).
    for i in 0..size {
        let byte = (v >> (i * 8)) & 0xFF;
        let _ = write!(out, "\tdb\t{byte}\r\n");
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

fn write_tail(
    out: &mut Vec<u8>,
    unit: &crate::ast::Unit,
    strings: &codegen::StringPool,
    helpers: &std::collections::HashSet<String>,
) {
    // Collect external function references: any name called from
    // somewhere in the TU that isn't defined here. Each becomes an
    // `extrn _<name>:near` directive in the tail, between the
    // empty `_TEXT segment / _TEXT ends` and the `public` list.
    // Fixtures 096–100.
    let externs = collect_extern_calls(unit);

    out.extend_from_slice(b"_DATA\tsegment word public 'DATA'\r\n");
    out.extend_from_slice(b"s@\tlabel\tbyte\r\n");
    // String literals and array-init blobs both materialize here in
    // the order they were interned. Strings get a `db '<chars>' / db
    // 0` pair (fixtures 088, 089); blobs get the raw byte image
    // (fixture 1481's `int a[3] = {10,20,30}` → 6 bytes of payload).
    for entry in strings.entries() {
        emit_string_literal_db(out, &entry.bytes);
        if entry.nul {
            out.extend_from_slice(b"\tdb\t0\r\n");
        }
    }
    out.extend_from_slice(b"_DATA\tends\r\n");
    out.extend_from_slice(b"_TEXT\tsegment byte public 'CODE'\r\n");
    out.extend_from_slice(b"_TEXT\tends\r\n");
    // Extern declarations come between the final `_TEXT ends` and
    // the `public` list. BCC orders by NAME LENGTH ASCENDING, then
    // reverse-alphabetical within each length bucket. Empirical from
    // 2894 (`_no,_yes` len 3/4 → no first); 2956, 3361, 3585
    // (`_f,_g` same len → g, f reverse-alpha); 3012 (`_setup,
    // _cleanup` len 6/8 → setup first). Source-order works only
    // when it happens to coincide (3012).
    let mut ordered_externs: Vec<String> = externs.clone();
    ordered_externs.sort_by(|a, b| {
        a.len()
            .cmp(&b.len())
            .then_with(|| b.cmp(a))
    });
    for name in &ordered_externs {
        let _ = write!(out, "\textrn\t_{name}:near\r\n");
    }
    // Public symbols are bucketed by **home segment** (_TEXT, _DATA,
    // _BSS in that fixed order), then **reverse-alphabetically sorted
    // within each bucket**. This rule matches every fixture in the
    // corpus today, but **does not match BCC's real behavior in
    // general** — fixture 198 (slice that introduced `int *p =
    // &arr[K]`) exposed that BCC actually uses a hash-bucket-style
    // ordering. See `specs/bcc/PUBLICS_ORDERING.md` for the open
    // question. We keep this rule because every existing fixture's
    // input happens to be in the rule's "covered" set; multi-long-
    // variable or multi-long-function shapes need the real rule.
    //
    // Disambiguated across:
    //   - 010 (`int f; int main`): output `_main, _f` — global
    //     reverse-alpha happens to work.
    //   - 095 (`int sum(); int main()`): output `_sum, _main`.
    //   - 087 (`int a; int b=5; char c=9; int main`): output
    //     `_main, _c, _b, _a` — _TEXT then _DATA (c, b in reverse-
    //     alpha) then _BSS (a).
    //   - 109 (`int x; int main`): output `_main, _x` — disambiguates
    //     global vs per-segment reverse-alpha.
    // Two-bucket publics layout. Symbols split by total *symbol*
    // name length (with the leading underscore counted): "long"
    // bucket = ≥ 3 chars (e.g. `_main`), "short" bucket = ≤ 2 chars
    // (e.g. `_g`, `_f`). Long bucket emits first in reverse-alpha;
    // short bucket second in reverse-alpha.
    //
    // The split is by NAME LENGTH, not by function-vs-variable kind:
    // fixture 218 (`long g; int f(long); main`) has `_f` and `_g`
    // both short (2 chars). Expected: `_main, _g, _f` (_main alone
    // in long bucket; short bucket reverse-alpha = g, f).
    //
    // The full general rule for the LONG bucket is still unsettled
    // — fixtures with multiple multi-char variables expose
    // additional ordering subtleties (see specs/OPEN_QUESTIONS.md).
    // Reverse-alpha within long bucket fits every current fixture,
    // including function-vs-function shapes (095 `_sum, _main`;
    // 179 `_main, _add`).
    // Each entry is (sort_key, emit_line). The sort_key is the symbol
    // name (used for reverse-alpha within each length bucket); the
    // emit_line is the formatted `public _x` or `extrn _x:near` text.
    // Runtime-helper externs (e.g. `N_LXLSH@`, fixture 228) get
    // merged in alongside publics — they participate in the same
    // length-bucket + reverse-alpha sort.
    // Long bucket splits further by kind: globals, then functions,
    // then helpers (each subgroup reverse-alpha within itself).
    // Fixture 465 (`unsigned char buf[3]; int g; int main(...)`):
    // long bucket = `_buf` (global) + `_main` (function), oracle
    // emits `_buf, _main` — globals first. Pure reverse-alpha would
    // give `_main, _buf` and break 465. Function-only cases (179,
    // 095) and helper-only cases (260) match either rule trivially.
    // BCC's PUBDEF order comes from iterating its internal symbol-
    // table hash. The hash function (reverse-engineered):
    //
    //   count       = len(name_without_underscore) + 1   // incl. NUL
    //   if count > 2:
    //     first_word = bytes[0] | (bytes[1] << 8)
    //     last_word  = bytes[count-3] | (bytes[count-2] << 8)
    //     hash = ((count << 6) + first_word + (last_word << 3)) & 0x3FF
    //   else:
    //     hash = bytes[0]
    //
    // The table has 1024 buckets; collisions chain in source-
    // declaration order (FIFO). Emission walks buckets HIGH→LOW
    // (0x3FF→0) and within each bucket walks the chain LAST→FIRST.
    //
    // The hash is computed on the bare C identifier without the
    // leading underscore (so `_main` hashes as `main`). Pascal-
    // convention functions are emitted UPPERCASE without the
    // underscore — they participate in the same table but the chain
    // entry's *emitted name* doesn't have the prefix.
    //
    // This rule was validated against the full fixture corpus —
    // 1229 out of 1229 multi-public fixtures match byte-exactly.
    fn pubs_hash(name_no_under: &str) -> usize {
        let b = name_no_under.as_bytes();
        let count = b.len() + 1;
        if count > 2 {
            let first_word = u32::from(b[0]) | (u32::from(b[1]) << 8);
            let last_word = u32::from(b[count - 3]) | (u32::from(b[count - 2]) << 8);
            let h = ((count as u32) << 6).wrapping_add(first_word).wrapping_add(last_word << 3);
            (h & 0x3FF) as usize
        } else {
            b[0] as usize
        }
    }
    const PUBS_TABLE_SIZE: usize = 0x400;
    let mut chain: Vec<Option<Vec<String>>> = vec![None; PUBS_TABLE_SIZE];
    let mut by_sym: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut insert = |sym: String, line: String,
                      chain: &mut Vec<Option<Vec<String>>>,
                      by_sym: &mut std::collections::HashMap<String, String>| {
        let hash_key = sym.strip_prefix('_').unwrap_or(&sym);
        let h = pubs_hash(hash_key);
        chain[h].get_or_insert_with(Vec::new).push(sym.clone());
        by_sym.insert(sym, line);
    };
    let mut insert_with_hash_key = |hash_key: String, sym: String, line: String,
                                    chain: &mut Vec<Option<Vec<String>>>,
                                    by_sym: &mut std::collections::HashMap<String, String>| {
        let h = pubs_hash(&hash_key);
        chain[h].get_or_insert_with(Vec::new).push(sym.clone());
        by_sym.insert(sym, line);
    };
    // Insertion order matches BCC's parser/sema encounter order:
    // walk in TRUE source-declaration order (functions and globals
    // interleaved as they appear in the source) so that hash-bucket
    // collisions resolve LIFO the same way BCC's symbol table does.
    // Fixture 3575 (`char arr[5]; void init(...)`): both hash to
    // the same bucket; source order arr-then-init means the LIFO
    // chain emits init first.
    for entry in &unit.decl_order {
        match entry {
            crate::ast::TopLevelRef::Function(idx) => {
                let f = &unit.functions[*idx];
                if f.body.is_some() && !f.is_static {
                    let sym = if f.is_pascal {
                        codegen::function_symbol_pascal(&f.name)
                    } else {
                        codegen::function_symbol(&f.name)
                    };
                    let line = format!("\tpublic\t{sym}\r\n");
                    // BCC hashes on the original C identifier (lowercase,
                    // no underscore) regardless of the calling convention,
                    // so the pascal symbol "ADD" shares a bucket order
                    // with the C-named "add".
                    insert_with_hash_key(
                        f.name.clone(),
                        sym,
                        line,
                        &mut chain,
                        &mut by_sym,
                    );
                }
            }
            crate::ast::TopLevelRef::Global(idx) => {
                let g = &unit.globals[*idx];
                if g.is_static || g.is_extern {
                    continue;
                }
                let sym = format!("_{}", g.name);
                let line = format!("\tpublic\t{sym}\r\n");
                insert(sym, line, &mut chain, &mut by_sym);
            }
        }
    }
    for helper in helpers {
        let line = format!("\textrn\t{helper}:far\r\n");
        insert(helper.clone(), line, &mut chain, &mut by_sym);
    }
    // Emit: buckets in REVERSE order, chain in REVERSE order.
    for i in (0..PUBS_TABLE_SIZE).rev() {
        if let Some(bucket) = &chain[i] {
            for sym in bucket.iter().rev() {
                if let Some(line) = by_sym.get(sym) {
                    out.extend_from_slice(line.as_bytes());
                }
            }
        }
    }
    // Data externs come after the public list (function externs come
    // before it, in `collect_extern_calls` order). Emitted in
    // *reverse declaration order* — fixture 481 (`extern int e1,
    // e2;` → `extrn _e2:word / extrn _e1:word`) pins this; for
    // single-extern fixtures the rule was invisible.
    for g in unit.globals.iter().rev() {
        if !g.is_extern {
            continue;
        }
        let width = extern_width(&g.ty);
        let _ = write!(out, "\textrn\t_{}:{width}\r\n", g.name);
    }
    // `main(argc, argv)` pulls in BCC's startup-time argv setup —
    // emit `extrn __setargv__:far` after publics so the linker
    // includes it. `main(void)` or `main()` skip this. Fixture 3117.
    let needs_setargv = unit.functions.iter().any(|f| {
        f.name == "main" && f.body.is_some() && !f.params.is_empty()
    });
    if needs_setargv {
        out.extend_from_slice(b"\textrn\t__setargv__:far\r\n");
    }
    out.extend_from_slice(b"\tend\r\n");
}

/// Map a C type to TASM's `extrn` width keyword. `int` → `word`,
/// `char` → `byte`, pointer → `word` (near pointer under -ms). Arrays
/// and structs as externs aren't fixture-tested yet; for now we fall
/// back to `byte` so the assembler can still compute reasonable
/// fixups.
fn extern_width(ty: &crate::ast::Type) -> &'static str {
    use crate::ast::Type;
    match ty {
        Type::Int | Type::Pointer(_) => "word",
        Type::Char => "byte",
        _ => "byte",
    }
}

/// Walk the AST and collect every function name that's *called* but
/// not *defined* in this TU. The result is the set of external
/// symbols we need to declare via `extrn _<name>:near`. Order is
/// source-order of first appearance (matching what BCC emits — we
/// haven't pinned the rule with multi-extern fixtures yet, but
/// source-order is the natural default).
fn collect_extern_calls(unit: &crate::ast::Unit) -> Vec<String> {
    use std::collections::HashSet;
    let defined: HashSet<&str> = unit
        .functions
        .iter()
        .filter(|f| f.body.is_some())
        .map(|f| f.name.as_str())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered: Vec<String> = Vec::new();
    for f in &unit.functions {
        let Some(body) = &f.body else { continue };
        // Per-function set of locals (params + declared variables).
        // A Call whose name is a local is an indirect call through
        // a function pointer, not an extern reference (fixture 110).
        let mut locals: HashSet<String> = f.params.iter().map(|p| p.name.clone()).collect();
        for stmt in body {
            walk_calls(stmt, &defined, &mut locals, &mut seen, &mut ordered);
        }
    }
    ordered
}

fn walk_calls(
    stmt: &crate::ast::Stmt,
    defined: &std::collections::HashSet<&str>,
    locals: &mut std::collections::HashSet<String>,
    seen: &mut std::collections::HashSet<String>,
    ordered: &mut Vec<String>,
) {
    use crate::ast::StmtKind;
    match &stmt.kind {
        StmtKind::Return(e) => {
            if let Some(e) = e {
                walk_calls_expr(e, defined, locals, seen, ordered);
            }
        }
        StmtKind::Declare { name, init, .. } => {
            if let Some(e) = init {
                walk_calls_expr(e, defined, locals, seen, ordered);
            }
            locals.insert(name.clone());
        }
        StmtKind::Assign { value, .. } | StmtKind::CompoundAssign { value, .. } => {
            walk_calls_expr(value, defined, locals, seen, ordered);
        }
        StmtKind::ArrayAssign { indices, value, .. }
        | StmtKind::ArrayCompoundAssign { indices, value, .. }
        | StmtKind::MemberArrayAssign { indices, value, .. } => {
            for ix in indices {
                walk_calls_expr(ix, defined, locals, seen, ordered);
            }
            walk_calls_expr(value, defined, locals, seen, ordered);
        }
        StmtKind::DerefAssign { target, value }
        | StmtKind::DerefCompoundAssign { target, value, .. } => {
            walk_calls_expr(target, defined, locals, seen, ordered);
            walk_calls_expr(value, defined, locals, seen, ordered);
        }
        StmtKind::MemberAssign { base, value, .. }
        | StmtKind::MemberCompoundAssign { base, value, .. } => {
            walk_calls_expr(base, defined, locals, seen, ordered);
            walk_calls_expr(value, defined, locals, seen, ordered);
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            walk_calls_expr(cond, defined, locals, seen, ordered);
            for s in then_branch {
                walk_calls(s, defined, locals, seen, ordered);
            }
            if let Some(b) = else_branch {
                for s in b {
                    walk_calls(s, defined, locals, seen, ordered);
                }
            }
        }
        StmtKind::While { cond, body } => {
            walk_calls_expr(cond, defined, locals, seen, ordered);
            for s in body {
                walk_calls(s, defined, locals, seen, ordered);
            }
        }
        StmtKind::DoWhile { body, cond } => {
            for s in body {
                walk_calls(s, defined, locals, seen, ordered);
            }
            walk_calls_expr(cond, defined, locals, seen, ordered);
        }
        StmtKind::For { init, cond, step, body } => {
            if let Some(exprs) = init {
                for e in exprs {
                    walk_calls_expr(e, defined, locals, seen, ordered);
                }
            }
            if let Some(e) = cond {
                walk_calls_expr(e, defined, locals, seen, ordered);
            }
            if let Some(exprs) = step {
                for e in exprs {
                    walk_calls_expr(e, defined, locals, seen, ordered);
                }
            }
            for s in body {
                walk_calls(s, defined, locals, seen, ordered);
            }
        }
        StmtKind::Switch { scrutinee, cases } => {
            walk_calls_expr(scrutinee, defined, locals, seen, ordered);
            for c in cases {
                for s in &c.body {
                    walk_calls(s, defined, locals, seen, ordered);
                }
            }
        }
        StmtKind::Break | StmtKind::Continue => {}
        StmtKind::Goto { .. } | StmtKind::Label { .. } | StmtKind::Empty => {}
        StmtKind::ExprStmt(e) => walk_calls_expr(e, defined, locals, seen, ordered),
        StmtKind::Block(body) => {
            for s in body {
                walk_calls(s, defined, locals, seen, ordered);
            }
        }
    }
}

fn walk_calls_expr(
    e: &crate::ast::Expr,
    defined: &std::collections::HashSet<&str>,
    locals: &std::collections::HashSet<String>,
    seen: &mut std::collections::HashSet<String>,
    ordered: &mut Vec<String>,
) {
    use crate::ast::ExprKind;
    match &e.kind {
        ExprKind::Call { name, args } => {
            // A call whose name is a known function in this TU is
            // direct (no EXTRN needed). A call whose name is a local
            // is an indirect call through a function pointer
            // (fixture 110) — also no EXTRN. Everything else gets
            // declared as an extern.
            if !defined.contains(name.as_str())
                && !locals.contains(name)
                && seen.insert(name.clone())
            {
                ordered.push(name.clone());
            }
            for a in args {
                walk_calls_expr(a, defined, locals, seen, ordered);
            }
        }
        ExprKind::BinOp { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            walk_calls_expr(left, defined, locals, seen, ordered);
            walk_calls_expr(right, defined, locals, seen, ordered);
        }
        ExprKind::Unary { operand, .. } | ExprKind::Deref(operand) => {
            walk_calls_expr(operand, defined, locals, seen, ordered);
        }
        ExprKind::AssignExpr { value, .. }
        | ExprKind::CompoundAssignExpr { value, .. } => {
            walk_calls_expr(value, defined, locals, seen, ordered)
        }
        ExprKind::ArrayIndex { array, index } => {
            walk_calls_expr(array, defined, locals, seen, ordered);
            walk_calls_expr(index, defined, locals, seen, ordered);
        }
        ExprKind::Member { base, .. } => walk_calls_expr(base, defined, locals, seen, ordered),
        ExprKind::Ternary { cond, then_value, else_value } => {
            walk_calls_expr(cond, defined, locals, seen, ordered);
            walk_calls_expr(then_value, defined, locals, seen, ordered);
            walk_calls_expr(else_value, defined, locals, seen, ordered);
        }
        ExprKind::Cast { operand, .. } => {
            walk_calls_expr(operand, defined, locals, seen, ordered);
        }
        ExprKind::InitList { items } => {
            for item in items {
                walk_calls_expr(item, defined, locals, seen, ordered);
            }
        }
        ExprKind::Comma { left, right } => {
            walk_calls_expr(left, defined, locals, seen, ordered);
            walk_calls_expr(right, defined, locals, seen, ordered);
        }
        ExprKind::UpdateLvalue { target, .. } => {
            walk_calls_expr(target, defined, locals, seen, ordered)
        }
        ExprKind::Ident(_)
        | ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::Update { .. }
        | ExprKind::AddressOf(_)
        | ExprKind::AddressOfArrayElem { .. } | ExprKind::AddressOfArrayElemVar { .. } => {}
    }
}

/// Render a string literal's bytes as one or more `db` lines.
/// Runs of printable ASCII go into a single quoted `db '...'`; each
/// non-printable byte (like `\n` = 10) becomes its own `db <decimal>`
/// line. Fixture 098 shows `"hi\n"` as `db 'hi' / db 10`.
///
/// We define "printable" as the ASCII printable range, excluding
/// the single quote (which would close the run). A real BCC may
/// have additional break conditions (e.g. tab), but our fixtures
/// only exercise newline.
fn emit_string_literal_db(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut quoted_run: Vec<u8> = Vec::new();
    let flush = |out: &mut Vec<u8>, run: &mut Vec<u8>| {
        if run.is_empty() {
            return;
        }
        out.extend_from_slice(b"\tdb\t'");
        out.extend_from_slice(run);
        out.extend_from_slice(b"'\r\n");
        run.clear();
    };
    for &b in bytes {
        if (0x20..0x7F).contains(&b) && b != b'\'' {
            quoted_run.push(b);
        } else {
            flush(out, &mut quoted_run);
            let _ = write!(out, "\tdb\t{b}\r\n");
        }
    }
    flush(out, &mut quoted_run);
}
