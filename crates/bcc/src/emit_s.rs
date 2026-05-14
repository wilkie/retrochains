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
    #[error("internal: ASM output is not valid UTF-8: {0}")]
    AsmNotUtf8(String),
    #[error("assemble: {0}")]
    Assemble(tasm::AsmError),
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
    // `_TEXT segment`. Fixtures 084, 086, 087. Externs declare storage
    // elsewhere, so they don't contribute to _DATA or _BSS.
    let has_init_globals = unit
        .globals
        .iter()
        .any(|g| !g.is_extern && g.init.is_some());
    if has_init_globals {
        write_init_globals(&mut out, &unit);
    }

    // _TEXT opens once for the whole translation unit; every function
    // *definition* lives inside (prototypes don't produce any asm).
    out.extend_from_slice(b"_TEXT\tsegment byte public 'CODE'\r\n");
    let signatures = codegen::Signatures::from_unit(&unit);
    let globals = codegen::GlobalTable::from_unit(&unit);
    let mut strings = codegen::StringPool::default();
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
        if g.is_extern {
            continue;
        }
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
        if g.is_extern || g.init.is_some() {
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
    // Collect external function references: any name called from
    // somewhere in the TU that isn't defined here. Each becomes an
    // `extrn _<name>:near` directive in the tail, between the
    // empty `_TEXT segment / _TEXT ends` and the `public` list.
    // Fixtures 096–100.
    let externs = collect_extern_calls(unit);

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
    // Extern declarations come between the final `_TEXT ends` and
    // the `public` list. Source order, one per called external.
    for name in &externs {
        let _ = write!(out, "\textrn\t_{name}:near\r\n");
    }
    // Public symbols are bucketed by **home segment** (_TEXT, _DATA,
    // _BSS in that fixed order), then **reverse-alphabetically sorted
    // within each bucket**.
    //
    // The rule was disambiguated across four fixtures:
    //   - 010 (`int f; int main` — both in _TEXT): output `_main, _f`
    //     — global reverse-alpha happens to work.
    //   - 095 (`int sum; int main` — both in _TEXT): output `_sum, _main`
    //     — global reverse-alpha works (main < sum → reverse = sum, main).
    //   - 087 (`int a; int b=5; char c=9; int main`): output
    //     `_main, _c, _b, _a` — _TEXT then _DATA (c, b in reverse-alpha)
    //     then _BSS (a). Global reverse-alpha would also produce this
    //     by coincidence.
    //   - 109 (`int x; int main`): output `_main, _x` — disambiguates.
    //     Global reverse-alpha would give `_x, _main` (since x > m).
    //     Per-segment reverse-alpha gives `_main` (in _TEXT) then `_x`
    //     (in _BSS) → correct.
    let mut text: Vec<String> = Vec::new();
    let mut data: Vec<String> = Vec::new();
    let mut bss: Vec<String> = Vec::new();
    for f in &unit.functions {
        if f.body.is_some() {
            text.push(codegen::function_symbol(&f.name));
        }
    }
    for g in &unit.globals {
        if g.is_static || g.is_extern {
            continue;
        }
        let bucket = if g.init.is_some() { &mut data } else { &mut bss };
        bucket.push(format!("_{}", g.name));
    }
    for bucket in [&mut text, &mut data, &mut bss] {
        bucket.sort();
        for name in bucket.iter().rev() {
            let _ = write!(out, "\tpublic\t{name}\r\n");
        }
    }
    // Data externs come after the public list (function externs come
    // before it, in `collect_extern_calls` order). Source order; the
    // width keyword (`word`/`byte`) is derived from the C type.
    for g in &unit.globals {
        if !g.is_extern {
            continue;
        }
        let width = extern_width(&g.ty);
        let _ = write!(out, "\textrn\t_{}:{width}\r\n", g.name);
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
        StmtKind::ArrayAssign { index, value, .. } => {
            walk_calls_expr(index, defined, locals, seen, ordered);
            walk_calls_expr(value, defined, locals, seen, ordered);
        }
        StmtKind::DerefAssign { target, value } => {
            walk_calls_expr(target, defined, locals, seen, ordered);
            walk_calls_expr(value, defined, locals, seen, ordered);
        }
        StmtKind::MemberAssign { base, value, .. } => {
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
            if let Some(e) = init {
                walk_calls_expr(e, defined, locals, seen, ordered);
            }
            if let Some(e) = cond {
                walk_calls_expr(e, defined, locals, seen, ordered);
            }
            if let Some(e) = step {
                walk_calls_expr(e, defined, locals, seen, ordered);
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
        StmtKind::ExprStmt(e) => walk_calls_expr(e, defined, locals, seen, ordered),
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
        ExprKind::AssignExpr { value, .. } => {
            walk_calls_expr(value, defined, locals, seen, ordered)
        }
        ExprKind::ArrayIndex { array, index } => {
            walk_calls_expr(array, defined, locals, seen, ordered);
            walk_calls_expr(index, defined, locals, seen, ordered);
        }
        ExprKind::Member { base, .. } => walk_calls_expr(base, defined, locals, seen, ordered),
        ExprKind::Ident(_)
        | ExprKind::IntLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::Update { .. }
        | ExprKind::AddressOf(_) => {}
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
