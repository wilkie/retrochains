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
    Assemble(bcc_tasm::AsmError),
}

/// Compile one `.C` source to `.ASM` next to it in the current directory.
///
/// # Errors
/// Returns [`EmitError`] on I/O failures, lex errors, or parse errors.
#[allow(clippy::needless_pass_by_value)]
pub fn emit_dash_s(
    source_path: &Path,
    memory_model: crate::cli::MemoryModel,
    merge_strings: bool,
    defines: &[(String, String)],
    unsigned_chars: bool,
    optimize: bool,
    target_186: bool,
    stack_check: bool,
    no_reg_vars: bool,
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

    let bytes = build_asm(&source, &lowered, mtime, memory_model, merge_strings, defines, unsigned_chars, optimize, target_186, stack_check, no_reg_vars)?;
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
    memory_model: crate::cli::MemoryModel,
    merge_strings: bool,
    defines: &[(String, String)],
    unsigned_chars: bool,
    optimize: bool,
    target_186: bool,
    stack_check: bool,
    no_reg_vars: bool,
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
    let mut unit = Parser::new(tokens).parse_unit()?;
    if unsigned_chars {
        promote_chars_to_uchar(&mut unit);
    }
    if memory_model.has_far_code() {
        // Medium / large / huge models compile every function as
        // far by default: the caller pushes a 4-byte segment:
        // offset return address, the prologue+epilogue lays out
        // the frame accordingly (`[bp+6]` is the first param, not
        // `[bp+4]`), and the function returns via `retf`. The
        // explicit `near` keyword in source overrides the
        // implicit-far promotion (fixture 2061: `int near
        // helper(...)` in medium model stays a near function with
        // `[bp+4]` params and `ret`). Fixtures 1665, 1667, 1685.
        for f in &mut unit.functions {
            if !f.is_near {
                f.is_far = true;
            }
        }
    }
    if memory_model.has_far_data() {
        // Compact / large / huge models default *data* pointers to
        // far: an unqualified `int *p` becomes `int far *p` (4-byte
        // slot, `les` / `es:[bx]` deref). The parser doesn't know
        // which model the codegen will run under, so it leaves every
        // unqualified pointer as a near `Pointer` and we rewrite
        // here. Function pointers (whose pointee is the
        // top-of-function dummy struct typedef) and explicitly
        // `near` pointers stay near. Fixtures 1667 (large) and
        // 1768 (compact).
        promote_data_pointers_to_far(&mut unit);
    } else {
        // Near-data models still need to collapse the parser's
        // `NearPointer` marker variant to plain `Pointer` so codegen
        // never sees it.
        collapse_explicit_near_to_pointer(&mut unit);
    }
    // Function pointers track *code* segment, so the model split is
    // `has_far_code()` (medium / large / huge) rather than the
    // far-data check above. Every `FnPointer` marker gets rewritten
    // to either `FarPointer { pointee: Int }` (far-code models —
    // 4-byte segment:offset slot) or `Pointer(Int)` (near-code —
    // 2-byte slot). After this pass codegen sees only the regular
    // pointer variants. Fixture 2211.
    rewrite_fn_pointers(&mut unit, memory_model.has_far_code());

    // Pseudo-registers (`_AX`, `_BX`, ..., `_DH`, `_SI`, ..., `_DS`)
    // are parsed as bare identifiers. Rewrite them to the dedicated
    // `ExprKind::PseudoReg` variant so the many `Ident`-matching
    // peephole recognizers (each of which would call `Locals::type_of`
    // and panic) don't fire. Codegen for `PseudoReg` lives in one
    // place per emit path. Fixtures 4051, 4053.
    rewrite_pseudo_registers(&mut unit);

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
            target_186,
            stack_check,
            no_reg_vars,
            memory_model.has_far_code(),
            matches!(memory_model, crate::cli::MemoryModel::Huge),
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

    let extern_dist = if memory_model.has_far_code() { "far" } else { "near" };
    write_tail(&mut out, &unit, &strings, &helpers, extern_dist);
    if optimize {
        fold_trampoline_jmps(&mut out);
    }
    if target_186 {
        fold_shl_to_multibit(&mut out);
    }
    if memory_model.has_far_code() {
        rename_text_segment_and_retn_to_retf(&mut out, source_filename_lower);
    }
    if matches!(memory_model, crate::cli::MemoryModel::Huge) {
        rewrite_for_huge_model(&mut out, source_filename_lower);
    }
    out.push(0x1A); // DOS EOF marker
    Ok(out)
}

/// For medium / large / huge memory models, BCC names the code
/// segment `<MODULE>_TEXT` (uppercased source basename) rather than
/// the canonical `_TEXT`. The far-return choice is now made by
/// per-function codegen (`is_far` is set on every TU function),
/// so this post-pass only renames the segment.
///
/// `source_filename_lower` is the lowercased source basename (e.g.
/// `hello.c`); the segment prefix uppercases the stem
/// (`HELLO_TEXT`). Fixtures 1664, 1666, 2052, 2053, 2061.
/// Huge memory model: post-process the asm output to merge `_DATA`
/// / `_BSS` into a single `<MODULE>_DATA` segment of class
/// `'FAR_DATA'`, drop the `DGROUP` group entirely (huge has no
/// shared data group across modules), and strip the `DGROUP:`
/// frame prefix from every data symbol reference (the assembler
/// then uses the symbol's own segment as its FIXUP frame). The
/// `d@` / `d@w` / `b@` / `b@w` size labels disappear too — they
/// were placeholders for the empty small-model scaffold and have
/// no analog in huge. Each function's prologue / epilogue already
/// learned the `push ds; mov ax, seg HELLO_DATA; mov ds, ax`
/// / `pop ds` pair via the `model_is_huge` codegen flag, so this
/// pass only handles the file-scope scaffolding. Fixtures 1770,
/// 2057.
fn rewrite_for_huge_model(out: &mut Vec<u8>, source_filename_lower: &str) {
    let stem = source_filename_lower
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("out");
    let module_upper = stem.to_ascii_uppercase();
    let data_seg = format!("{module_upper}_DATA");
    let original = std::mem::take(out);
    let text = String::from_utf8(original).expect("asm output is ASCII");
    let mut rewritten = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start_matches('\t');
        let leading_tabs = line.len() - trimmed.len();
        // Drop the `DGROUP\tgroup\t...` directive entirely — huge
        // does not group its data segments.
        if trimmed.starts_with("DGROUP\tgroup\t") {
            continue;
        }
        // Drop the `d@` / `d@w` / `b@` / `b@w` size labels — the
        // huge scaffold doesn't emit them.
        if trimmed == "d@\tlabel\tbyte\r\n"
            || trimmed == "d@w\tlabel\tword\r\n"
            || trimmed == "b@\tlabel\tbyte\r\n"
            || trimmed == "b@w\tlabel\tword\r\n"
        {
            continue;
        }
        // `assume\tcs:HELLO_TEXT,ds:DGROUP` → drop the `,ds:DGROUP`
        // tail; under huge the ds value is established per-function.
        let mut s: String = if trimmed.starts_with("assume\tcs:") && trimmed.contains(",ds:DGROUP") {
            line.replace(",ds:DGROUP", "")
        } else {
            line.to_string()
        };
        // Rename `_DATA` / `_BSS` segment lines (segment / ends /
        // class). Small-model scaffold has both; huge folds them
        // into a single `<MODULE>_DATA` of class `'FAR_DATA'`.
        if s.contains("_DATA\tsegment") || s.contains("_DATA\tends") {
            s = s.replace("_DATA\t", &format!("{data_seg}\t"));
            s = s.replace("'DATA'", "'FAR_DATA'");
        }
        if s.contains("_BSS\tsegment") || s.contains("_BSS\tends") {
            s = s.replace("_BSS\t", &format!("{data_seg}\t"));
            s = s.replace("'BSS'", "'FAR_DATA'");
        }
        // Strip the `DGROUP:` frame prefix from symbol references —
        // `DGROUP:_g` → `_g`, `offset DGROUP:_g` → `offset _g`. The
        // assembler then uses the symbol's own segment for the
        // FIXUP frame.
        if s.contains("DGROUP:") {
            s = s.replace("DGROUP:", "");
        }
        let _ = leading_tabs;
        rewritten.push_str(&s);
    }
    *out = rewritten.into_bytes();
}

fn rename_text_segment_and_retn_to_retf(out: &mut Vec<u8>, source_filename_lower: &str) {
    let stem = source_filename_lower
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("out");
    let module_upper = stem.to_ascii_uppercase();
    let new_seg = format!("{module_upper}_TEXT");
    let original = std::mem::take(out);
    let text = String::from_utf8(original).expect("asm output is ASCII");
    let mut rewritten = String::with_capacity(text.len() + 64);
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start_matches('\t');
        let leading_tabs = line.len() - trimmed.len();
        let replaced = if trimmed.starts_with("_TEXT\tsegment") || trimmed.starts_with("_TEXT\tends") {
            let body = &trimmed[5..];
            let mut s = String::with_capacity(leading_tabs + new_seg.len() + body.len());
            for _ in 0..leading_tabs {
                s.push('\t');
            }
            s.push_str(&new_seg);
            s.push_str(body);
            s
        } else if let Some(idx) = line.find("cs:_TEXT") {
            let mut s = String::with_capacity(line.len() + new_seg.len());
            s.push_str(&line[..idx + 3]);
            s.push_str(&new_seg);
            s.push_str(&line[idx + 3 + 5..]);
            s
        } else {
            line.to_owned()
        };
        rewritten.push_str(&replaced);
    }
    *out = rewritten.into_bytes();
}

/// `-1`/`-2` peephole: collapse runs of `\tshl\t<reg16>,1\r\n` of
/// length ≥ 3 into a single `\tshl\t<reg16>,N\r\n` (the 80186+ multi-
/// bit shift form, encoding `C1 /4 ib` — 3 bytes total). Also rewrite
/// the 8086 `mov cl, K; shl r16, cl` pair (4 bytes) into the same
/// form. Saves bytes per shift on 186/286 targets without changing
/// the result. Fixtures 2133 (`x*8`), 2134 (`x*16`), 2276 (`x<<4`).
fn fold_shl_to_multibit(out: &mut Vec<u8>) {
    let text = std::mem::take(out);
    let s = match String::from_utf8(text) {
        Ok(s) => s,
        Err(e) => {
            *out = e.into_bytes();
            return;
        }
    };
    let mut lines: Vec<String> = s.split_inclusive('\n').map(String::from).collect();
    let mut keep: Vec<bool> = vec![true; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        // Pass 1: detect `\tmov\tcl,K\r\n\tshl\t<reg16>,cl\r\n` pair.
        let pair_match: Option<(u8, String)> = {
            let cur_k = lines[i].strip_prefix("\tmov\tcl,").and_then(|rest| {
                rest.trim_end_matches(['\r', '\n']).parse::<u8>().ok()
            });
            if let Some(k) = cur_k
                && k >= 2
                && k <= 31
                && i + 1 < lines.len()
            {
                lines[i + 1].strip_prefix("\tshl\t").and_then(|shl_rest| {
                    let reg = shl_rest.trim_end_matches(['\r', '\n']).strip_suffix(",cl")?;
                    if is_reg16_name(reg) { Some((k, reg.to_string())) } else { None }
                })
            } else {
                None
            }
        };
        if let Some((k, reg)) = pair_match {
            lines[i].clear();
            keep[i] = false;
            lines[i + 1] = format!("\tshl\t{reg},{k}\r\n");
            i += 2;
            continue;
        }
        // Pass 2: detect a run of `\tshl\t<reg16>,1\r\n`.
        let run_reg: Option<String> = lines[i].strip_prefix("\tshl\t").and_then(|sr| {
            let reg = sr.trim_end_matches(['\r', '\n']).strip_suffix(",1")?;
            if is_reg16_name(reg) { Some(reg.to_string()) } else { None }
        });
        if let Some(reg) = run_reg {
            let mut run = 1usize;
            while i + run < lines.len() {
                let m = lines[i + run].strip_prefix("\tshl\t").and_then(|sr| {
                    let r2 = sr.trim_end_matches(['\r', '\n']).strip_suffix(",1")?;
                    if r2 == reg { Some(()) } else { None }
                });
                if m.is_some() {
                    run += 1;
                } else {
                    break;
                }
            }
            if run >= 3 {
                lines[i] = format!("\tshl\t{reg},{run}\r\n");
                for k in 1..run {
                    keep[i + k] = false;
                }
                i += run;
                continue;
            }
        }
        i += 1;
    }
    for (j, k) in keep.iter().enumerate() {
        if !k {
            lines[j].clear();
        }
    }
    let joined: String = lines.concat();
    *out = joined.into_bytes();
}

fn is_reg16_name(s: &str) -> bool {
    matches!(s, "ax" | "bx" | "cx" | "dx" | "si" | "di" | "bp" | "sp")
}

/// `-O` peephole: drop `\tjmp\tshort @X\r\n` lines that are
/// immediately followed by `@X:\r\n` (with at most blank or
/// `;`-comment lines between). The jmp would land at the next
/// instruction anyway, so removing it saves 2 bytes per occurrence
/// without changing semantics. Fixtures 2125, 2126, 2281.
fn fold_trampoline_jmps(out: &mut Vec<u8>) {
    // Work line-by-line through the buffer. We mutate in place by
    // copying lines we keep into a new buffer.
    let text = std::mem::take(out);
    let s = match std::str::from_utf8(&text) {
        Ok(s) => s,
        Err(_) => {
            *out = text;
            return;
        }
    };
    let mut lines: Vec<&str> = s.split_inclusive('\n').collect();
    // Walk in reverse so we can match `jmp short @X` against the
    // first non-blank-non-comment label line that follows.
    let mut keep: Vec<bool> = vec![true; lines.len()];
    for i in 0..lines.len() {
        let line = lines[i];
        // Match `\tjmp\tshort @<label>\r\n`.
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let Some(rest) = trimmed.strip_prefix("\tjmp\tshort ") else { continue };
        // Look forward for the first non-blank, non-comment line.
        for j in (i + 1)..lines.len() {
            let next = lines[j].trim_end_matches(['\r', '\n']);
            // Skip blank lines (whole-line whitespace) and `;`
            // comment-only lines that codegen emits between stmts.
            let trimmed_next = next.trim_start();
            if trimmed_next.is_empty() || trimmed_next.starts_with(';') {
                continue;
            }
            // Does this line declare exactly the label we jump to?
            // Labels are written as `<label>:`.
            if next == format!("{rest}:") {
                keep[i] = false;
            }
            break;
        }
    }
    for (i, keep) in keep.iter().enumerate() {
        if !keep {
            lines[i] = "";
        }
    }
    let joined: String = lines.concat();
    *out = joined.into_bytes();
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

/// BCC's reverse-engineered symbol-table hash (1024 buckets). Computed on the
/// bare C identifier WITHOUT a leading underscore (`_main` hashes as `main`).
/// Drives the publics order (HIGH→LOW buckets, LAST→FIRST chain), the function-
/// extern order (same direction), and the `_BSS` global order (the REVERSE walk:
/// LOW→HIGH buckets, FIRST→FIRST chain). Validated against the full corpus plus
/// the 91xx/92xx discriminator fixtures.
pub(crate) fn pubs_hash(name_no_under: &str) -> usize {
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

/// Emit uninitialized globals in `_BSS` at the bottom of the file,
/// with the function-end `?debug C E9` record placed before
/// `_BSS ends` (fixture 087).
fn write_bss_globals_with_debug(out: &mut Vec<u8>, unit: &crate::ast::Unit) {
    out.extend_from_slice(b"_BSS\tsegment word public 'BSS'\r\n");
    // BCC's _BSS layout orders globals by the SAME 1024-bucket symbol-table
    // hash as the publics/externs (`pubs_hash`), but walked in the EXACT
    // REVERSE of the publics emission: buckets LOW→HIGH and within each bucket
    // the collision chain FIRST→LAST (source order). The 1-byte filler `db 1
    // dup (?)` is inserted when the running offset is odd before a word-aligned
    // global.
    //
    // The previous "short bucket (≤1-char) alpha, then long bucket source
    // order" rule was a coincidental approximation: it agreed with the hash on
    // every prior fixture (181/462/234 single-char names hash to their ASCII =
    // ascending; 465 `g`(103)<`buf`(522); 3059 `src`(771)<`dst`(1020); 1401
    // `s1`(459)<`s2`(715) — all happened to match) but is wrong in general.
    // Pinned by oracle-captured 4151 (`foo`,`bar` → bar,foo not foo,bar),
    // 4152 (`cd`,`z` → cd,z — a 2-char name sorts BEFORE a 1-char one, breaking
    // the short-bucket-first rule), 4153 (`foo`,`bar`,`qux` → bar,qux,foo),
    // 4154 (`bar`,`baz` collide @362 → bar,baz, FIRST→LAST chain).
    let mut bss_chain: Vec<Option<Vec<&crate::ast::Global>>> = vec![None; 0x400];
    for g in unit.globals.iter().filter(|g| !g.is_extern && g.init.is_none()) {
        let h = pubs_hash(g.name.strip_prefix('_').unwrap_or(&g.name));
        bss_chain[h].get_or_insert_with(Vec::new).push(g);
    }
    let bss: Vec<&crate::ast::Global> =
        bss_chain.into_iter().flatten().flatten().collect();
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
    // `char *p = "lit"` under compact / large models — the pointer
    // promotion pass has rewritten `p` to a 4-byte far pointer. BCC
    // emits a `dd DGROUP:s@[+N]` so the linker writes both the
    // string's DGROUP-relative offset and DGROUP's paragraph value.
    // Fixtures 3760 / 3761.
    if let (ExprKind::StringLit(bytes), Type::FarPointer { pointee, .. }) =
        (&init.kind, ty)
    {
        if (*pointee).is_char_like() {
            let offset = strings.intern(bytes);
            if offset == 0 {
                out.extend_from_slice(b"\tdd\tDGROUP:s@\r\n");
            } else {
                let _ = write!(out, "\tdd\tDGROUP:s@+{offset}\r\n");
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
    // `T *p = &g;` for compact/large where the pointer was promoted
    // to far. Emit `dd DGROUP:_<target>` so the linker writes the
    // 4-byte far pointer (offset:segment). Fixtures 3900 / 3901.
    if let (ExprKind::AddressOf(target_name), Type::FarPointer { .. }) =
        (&init.kind, ty)
    {
        let _ = write!(out, "\tdd\tDGROUP:_{target_name}\r\n");
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

/// Rewrite plain `char` to `unsigned char` everywhere in the AST.
/// Implements BCC's `-K` flag: with the flag set, every `char`
/// declaration that wasn't explicitly `signed char` becomes
/// `unsigned char`. Affects widening (`mov ah, 0` vs `cbw`) and
/// signed-vs-unsigned compares. Fixtures 2130, 2284.
/// Compact / large / huge memory models default data pointers to
/// far. Rewrite every `Type::Pointer(p)` reachable from the unit
/// (globals, function returns, params, local declares) to
/// `Type::FarPointer { pointee: p, is_huge: false }` (plain `far`;
/// `huge` is still opt-in via explicit keyword in source, since its
/// arithmetic semantics differ). Nested pointers (`int **pp`) get
/// promoted on each level so a `*pp` deref through the outer
/// pointer also reads a far pointer back out of memory. Fixtures
/// 1667 (large), 1768 (compact); future Huge-model fixtures follow
/// the same shape.
/// Rewrite every `FnPointer` marker the parser stamped onto a
/// function-pointer declarator. In far-code memory models (medium,
/// large, huge) the slot becomes a 4-byte `FarPointer` so the
/// codegen emits the `mov [bp+hi], cs; mov [bp+lo], offset _fn`
/// init pair and the `call far ptr [bp+off]` indirect-call. In
/// near-code models the slot collapses to a regular 2-byte
/// `Pointer(Int)` — the function lives in the same code segment as
/// every other function in the module, so an indirect near call
/// suffices. Fixture 2211 (medium fn-ptr), fixture 110 (small).
fn rewrite_fn_pointers(unit: &mut crate::ast::Unit, far_code: bool) {
    use crate::ast::Type;
    fn walk_ty(t: &mut Type, far_code: bool) {
        match t {
            Type::FnPointer => {
                *t = if far_code {
                    Type::FarPointer { pointee: Box::new(Type::Int), is_huge: false }
                } else {
                    Type::Pointer(Box::new(Type::Int))
                };
            }
            Type::Array { elem, .. } => walk_ty(elem, far_code),
            Type::Pointer(inner) | Type::NearPointer(inner) => walk_ty(inner, far_code),
            Type::FarPointer { pointee, .. } => walk_ty(pointee, far_code),
            Type::Struct { fields, .. } => {
                for f in fields {
                    walk_ty(&mut f.ty, far_code);
                }
            }
            _ => {}
        }
    }
    for g in &mut unit.globals {
        walk_ty(&mut g.ty, far_code);
    }
    for f in &mut unit.functions {
        walk_ty(&mut f.ret_ty, far_code);
        for p in &mut f.params {
            walk_ty(&mut p.ty, far_code);
        }
        if let Some(body) = &mut f.body {
            for s in body {
                walk_stmt(s, far_code);
            }
        }
    }
    fn walk_stmt(s: &mut crate::ast::Stmt, far_code: bool) {
        use crate::ast::StmtKind;
        match &mut s.kind {
            StmtKind::Declare { ty, .. } => walk_ty(ty, far_code),
            StmtKind::Block(b) => for inner in b { walk_stmt(inner, far_code); }
            StmtKind::If { then_branch, else_branch, .. } => {
                for s in then_branch { walk_stmt(s, far_code); }
                if let Some(eb) = else_branch { for s in eb { walk_stmt(s, far_code); } }
            }
            StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
                for s in body { walk_stmt(s, far_code); }
            }
            StmtKind::For { body, .. } => {
                for s in body { walk_stmt(s, far_code); }
            }
            StmtKind::Switch { cases, .. } => {
                for c in cases { for s in &mut c.body { walk_stmt(s, far_code); } }
            }
            _ => {}
        }
    }
}

/// Rewrite every `Ident(name)` where `name` names a pseudo-register
/// (`_AX`, `_BX`, ..., `_DS`) into the dedicated
/// `ExprKind::PseudoReg(name)` variant. After this pass, no Ident
/// node in the unit references a pseudo-register, so the many
/// `Ident`-matching peephole recognizers stop firing on them
/// (each would call `Locals::type_of` and panic).
fn rewrite_pseudo_registers(unit: &mut crate::ast::Unit) {
    for g in &mut unit.globals {
        if let Some(init) = &mut g.init {
            walk_expr_pseudo(init);
        }
    }
    for f in &mut unit.functions {
        if let Some(body) = &mut f.body {
            for s in body {
                walk_stmt_pseudo(s);
            }
        }
    }
}

fn walk_stmt_pseudo(s: &mut crate::ast::Stmt) {
    use crate::ast::StmtKind;
    match &mut s.kind {
        StmtKind::Return(Some(e)) => walk_expr_pseudo(e),
        StmtKind::Declare { init: Some(e), .. } => walk_expr_pseudo(e),
        StmtKind::Assign { value, .. } | StmtKind::CompoundAssign { value, .. } => {
            walk_expr_pseudo(value);
        }
        StmtKind::ExprStmt(e) => walk_expr_pseudo(e),
        StmtKind::ArrayAssign { indices, value, .. }
        | StmtKind::ArrayCompoundAssign { indices, value, .. }
        | StmtKind::MemberArrayAssign { indices, value, .. } => {
            for i in indices {
                walk_expr_pseudo(i);
            }
            walk_expr_pseudo(value);
        }
        StmtKind::DerefAssign { target, value }
        | StmtKind::DerefCompoundAssign { target, value, .. } => {
            walk_expr_pseudo(target);
            walk_expr_pseudo(value);
        }
        StmtKind::MemberAssign { base, value, .. }
        | StmtKind::MemberCompoundAssign { base, value, .. } => {
            walk_expr_pseudo(base);
            walk_expr_pseudo(value);
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            walk_expr_pseudo(cond);
            for s in then_branch {
                walk_stmt_pseudo(s);
            }
            if let Some(eb) = else_branch {
                for s in eb {
                    walk_stmt_pseudo(s);
                }
            }
        }
        StmtKind::While { cond, body } | StmtKind::DoWhile { cond, body } => {
            walk_expr_pseudo(cond);
            for s in body {
                walk_stmt_pseudo(s);
            }
        }
        StmtKind::For { init, cond, step, body } => {
            if let Some(es) = init {
                for e in es {
                    walk_expr_pseudo(e);
                }
            }
            if let Some(e) = cond {
                walk_expr_pseudo(e);
            }
            if let Some(es) = step {
                for e in es {
                    walk_expr_pseudo(e);
                }
            }
            for s in body {
                walk_stmt_pseudo(s);
            }
        }
        StmtKind::Switch { scrutinee, cases } => {
            walk_expr_pseudo(scrutinee);
            for c in cases {
                for s in &mut c.body {
                    walk_stmt_pseudo(s);
                }
            }
        }
        StmtKind::Block(body) => {
            for s in body {
                walk_stmt_pseudo(s);
            }
        }
        StmtKind::Return(None)
        | StmtKind::Declare { init: None, .. }
        | StmtKind::Empty
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Goto { .. }
        | StmtKind::Label { .. }
        | StmtKind::Asm { .. } => {}
    }
}

fn walk_expr_pseudo(e: &mut crate::ast::Expr) {
    use crate::ast::ExprKind;
    if let ExprKind::Ident(name) = &e.kind
        && codegen::is_asm_pseudo_register(name)
    {
        let ExprKind::Ident(taken) = std::mem::replace(&mut e.kind, ExprKind::IntLit(0)) else {
            unreachable!()
        };
        e.kind = ExprKind::PseudoReg(taken);
        return;
    }
    match &mut e.kind {
        ExprKind::BinOp { left, right, .. }
        | ExprKind::Logical { left, right, .. }
        | ExprKind::Comma { left, right } => {
            walk_expr_pseudo(left);
            walk_expr_pseudo(right);
        }
        ExprKind::Unary { operand, .. } | ExprKind::Cast { operand, .. } => {
            walk_expr_pseudo(operand);
        }
        ExprKind::Deref(inner) => walk_expr_pseudo(inner),
        ExprKind::UpdateLvalue { target, .. } => walk_expr_pseudo(target),
        ExprKind::AssignExpr { value, .. } => walk_expr_pseudo(value),
        ExprKind::AssignLvalueExpr { target, value } => {
            walk_expr_pseudo(target);
            walk_expr_pseudo(value);
        }
        ExprKind::CompoundAssignExpr { value, .. } => walk_expr_pseudo(value),
        ExprKind::Call { args, .. } => {
            for a in args {
                walk_expr_pseudo(a);
            }
        }
        ExprKind::CallVia { addr, args } => {
            walk_expr_pseudo(addr);
            for a in args {
                walk_expr_pseudo(a);
            }
        }
        ExprKind::ArrayIndex { array, index } => {
            walk_expr_pseudo(array);
            walk_expr_pseudo(index);
        }
        ExprKind::Member { base, .. } => walk_expr_pseudo(base),
        ExprKind::Ternary { cond, then_value, else_value } => {
            walk_expr_pseudo(cond);
            walk_expr_pseudo(then_value);
            walk_expr_pseudo(else_value);
        }
        ExprKind::InitList { items } => {
            for i in items {
                walk_expr_pseudo(i);
            }
        }
        ExprKind::Ident(_)
        | ExprKind::PseudoReg(_)
        | ExprKind::Update { .. }
        | ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::AddressOf(_)
        | ExprKind::AddressOfArrayElem { .. }
        | ExprKind::AddressOfArrayElemVar { .. }
        | ExprKind::StringLit(_) => {}
    }
}

/// Near-data memory models (tiny / small / medium) don't promote
/// pointers, but the parser still produces `NearPointer` for an
/// explicit `near` qualifier. Collapse those back to plain
/// `Pointer` so codegen sees a single shape.
fn collapse_explicit_near_to_pointer(unit: &mut crate::ast::Unit) {
    use crate::ast::Type;
    fn walk_ty(t: &mut Type) {
        match t {
            Type::NearPointer(inner) => {
                walk_ty(inner);
                let pointee = std::mem::replace(inner.as_mut(), Type::Int);
                *t = Type::Pointer(Box::new(pointee));
            }
            Type::Pointer(inner) => walk_ty(inner),
            Type::FarPointer { pointee, .. } => walk_ty(pointee),
            Type::Array { elem, .. } => walk_ty(elem),
            Type::Struct { fields, .. } => {
                for f in fields {
                    walk_ty(&mut f.ty);
                }
            }
            _ => {}
        }
    }
    for g in &mut unit.globals {
        walk_ty(&mut g.ty);
    }
    for f in &mut unit.functions {
        walk_ty(&mut f.ret_ty);
        for p in &mut f.params {
            walk_ty(&mut p.ty);
        }
        if let Some(body) = &mut f.body {
            for s in body {
                walk_stmt(s);
            }
        }
    }
    fn walk_stmt(s: &mut crate::ast::Stmt) {
        use crate::ast::StmtKind;
        match &mut s.kind {
            StmtKind::Declare { ty, .. } => walk_ty(ty),
            StmtKind::Block(b) => for inner in b { walk_stmt(inner); }
            StmtKind::If { then_branch, else_branch, .. } => {
                for s in then_branch { walk_stmt(s); }
                if let Some(eb) = else_branch { for s in eb { walk_stmt(s); } }
            }
            StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
                for s in body { walk_stmt(s); }
            }
            StmtKind::For { body, .. } => {
                for s in body { walk_stmt(s); }
            }
            StmtKind::Switch { cases, .. } => {
                for c in cases { for s in &mut c.body { walk_stmt(s); } }
            }
            _ => {}
        }
    }
}

fn promote_data_pointers_to_far(unit: &mut crate::ast::Unit) {
    use crate::ast::Type;
    fn walk_ty(t: &mut Type) {
        match t {
            Type::Pointer(inner) => {
                walk_ty(inner);
                let pointee = std::mem::replace(inner.as_mut(), Type::Int);
                *t = Type::FarPointer {
                    pointee: Box::new(pointee),
                    is_huge: false,
                };
            }
            Type::NearPointer(inner) => {
                // Explicitly `near` — collapse back to a regular
                // near Pointer without promoting. Fixture 1748.
                walk_ty(inner);
                let pointee = std::mem::replace(inner.as_mut(), Type::Int);
                *t = Type::Pointer(Box::new(pointee));
            }
            Type::FarPointer { pointee, .. } => walk_ty(pointee),
            Type::Array { elem, .. } => walk_ty(elem),
            Type::Struct { fields, .. } => {
                for f in fields {
                    walk_ty(&mut f.ty);
                }
            }
            _ => {}
        }
    }
    for g in &mut unit.globals {
        walk_ty(&mut g.ty);
    }
    for f in &mut unit.functions {
        walk_ty(&mut f.ret_ty);
        for p in &mut f.params {
            walk_ty(&mut p.ty);
        }
        if let Some(body) = &mut f.body {
            for s in body {
                walk_stmt(s);
            }
        }
    }
    fn walk_stmt(s: &mut crate::ast::Stmt) {
        use crate::ast::StmtKind;
        match &mut s.kind {
            StmtKind::Declare { ty, .. } => walk_ty(ty),
            StmtKind::Block(b) => {
                for inner in b {
                    walk_stmt(inner);
                }
            }
            StmtKind::If { then_branch, else_branch, .. } => {
                for s in then_branch {
                    walk_stmt(s);
                }
                if let Some(else_b) = else_branch {
                    for s in else_b {
                        walk_stmt(s);
                    }
                }
            }
            StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
                for s in body {
                    walk_stmt(s);
                }
            }
            StmtKind::For { body, .. } => {
                for s in body {
                    walk_stmt(s);
                }
            }
            StmtKind::Switch { cases, .. } => {
                for c in cases {
                    for s in &mut c.body {
                        walk_stmt(s);
                    }
                }
            }
            _ => {}
        }
    }
}

fn promote_chars_to_uchar(unit: &mut crate::ast::Unit) {
    use crate::ast::Type;
    fn walk_ty(t: &mut Type) {
        match t {
            Type::Char => *t = Type::UChar,
            Type::Array { elem, .. } => walk_ty(elem),
            Type::Pointer(p) => walk_ty(p),
            Type::Struct { fields, .. } => {
                for f in fields {
                    walk_ty(&mut f.ty);
                }
            }
            _ => {}
        }
    }
    for g in &mut unit.globals {
        walk_ty(&mut g.ty);
    }
    for f in &mut unit.functions {
        walk_ty(&mut f.ret_ty);
        for p in &mut f.params {
            walk_ty(&mut p.ty);
        }
        if let Some(body) = &mut f.body {
            for s in body {
                walk_stmt(s);
            }
        }
    }
    fn walk_stmt(s: &mut crate::ast::Stmt) {
        use crate::ast::StmtKind;
        match &mut s.kind {
            StmtKind::Declare { ty, .. } => walk_ty(ty),
            StmtKind::Block(b) => {
                for inner in b {
                    walk_stmt(inner);
                }
            }
            StmtKind::If { then_branch, else_branch, .. } => {
                for s in then_branch {
                    walk_stmt(s);
                }
                if let Some(else_b) = else_branch {
                    for s in else_b {
                        walk_stmt(s);
                    }
                }
            }
            StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
                for s in body {
                    walk_stmt(s);
                }
            }
            StmtKind::For { body, .. } => {
                for s in body {
                    walk_stmt(s);
                }
            }
            StmtKind::Switch { cases, .. } => {
                for c in cases {
                    for s in &mut c.body {
                        walk_stmt(s);
                    }
                }
            }
            _ => {}
        }
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
    extern_dist: &str,
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
    // Extern (function EXTDEF) declarations come between the final
    // `_TEXT ends` and the `public` list. BCC orders them by its
    // symbol-table hash — the SAME 1024-bucket hash as the publics
    // (see `pubs_hash` below): collisions chain in first-reference
    // order (FIFO, as `collect_extern_calls` returns them), emission
    // walks buckets HIGH→LOW and each chain LAST→FIRST. The hash is on
    // the bare name (no leading underscore).
    //
    // The earlier rule (name length ASC, reverse-alpha within ties)
    // was a coincidental approximation: it agreed with the hash on all
    // 9 prior multi-extern fixtures but is WRONG in general. Pinned by
    // oracle-captured 4148 (`cd`,`foo` → foo,cd, not cd,foo), 4149 (`ab`,`cd`
    // → ab,cd, not cd,ab), 4150 (`ab`,`cd`,`foo` → foo,ab,cd). Also explains
    // the long-standing fopen/fclose anomaly (fclose before fopen) the length
    // rule could not.
    let mut ext_chain: Vec<Option<Vec<String>>> = vec![None; 0x400];
    for name in &externs {
        let h = pubs_hash(name);
        ext_chain[h].get_or_insert_with(Vec::new).push(name.clone());
    }
    let mut ordered_externs: Vec<String> = Vec::new();
    for bucket in ext_chain.into_iter().rev() {
        if let Some(names) = bucket {
            for n in names.into_iter().rev() {
                ordered_externs.push(n);
            }
        }
    }
    for name in &ordered_externs {
        let _ = write!(out, "\textrn\t_{name}:{extern_dist}\r\n");
    }
    // Floating-point runtime markers (`FIWRQQ`, `FIDRQQ`) go BETWEEN
    // user-function externs and runtime helpers in BCC's EXTDEF
    // order. Scan the already-emitted asm bytes for the relevant
    // instruction mnemonics so we can decide whether to emit them.
    // Fixtures 1670/1678/2195 pinned this placement.
    let asm_text = std::str::from_utf8(out).unwrap_or("");
    let uses_fwait = asm_text.contains("\tfwait\t");
    let uses_fpu = uses_fwait
        || asm_text.contains("\tfld\t")
        || asm_text.contains("\tfstp\t")
        || asm_text.contains("\tfadd\t")
        || asm_text.contains("\tfsub\t")
        || asm_text.contains("\tfmul\t")
        || asm_text.contains("\tfdiv\t")
        || asm_text.contains("\tfcomp\t")
        || asm_text.contains("\tfcompp\t")
        || asm_text.contains("\tfldz\t")
        || asm_text.contains("\tfld1\t")
        || asm_text.contains("\tfchs\t")
        || asm_text.contains("\tfstsw\t")
        || asm_text.contains("\tfild\t");
    if uses_fwait {
        out.extend_from_slice(b"\textrn\tFIWRQQ:far\r\n");
    }
    if uses_fpu {
        out.extend_from_slice(b"\textrn\tFIDRQQ:far\r\n");
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
    // `pubs_hash` is now a module-level fn (shared with the extern and
    // _BSS orderings).
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
    // Helpers (runtime-library externs like `N_LXLSH@`,
    // `N_OVERFLOW@`, `___brklvl`) are emitted BEFORE the publics-
    // bucket walk in ASCII-sorted order. BCC's EXTDEF order is not
    // covered by the publics-bucket hash; sorting alphabetically
    // matches both single-helper and multi-helper fixtures (228,
    // 2129). Fixture 2129 (`-N` stack check) pinned this rule.
    let mut helper_sorted: Vec<&String> = helpers.iter().collect();
    helper_sorted.sort();
    for helper in helper_sorted {
        // `___brklvl` is a word-sized data extern (the runtime's
        // stack-break sentinel); everything else in the helper bag
        // is a far-call target like `N_LXLSH@`, `N_OVERFLOW@`, etc.
        let width = if helper == "___brklvl" { "word" } else { "far" };
        let _ = write!(out, "\textrn\t{helper}:{width}\r\n");
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
    // Globals that are pointer-typed act as function pointers when
    // called — `op()` for `int (*op)(int);` is an indirect call, not
    // an extern reference (fixtures 2607, 2913).
    let global_fnptrs: HashSet<&str> = unit
        .globals
        .iter()
        .filter(|g| g.ty.pointee().is_some())
        .map(|g| g.name.as_str())
        .collect();
    // Function prototypes declared but not defined here. References
    // to these (in initializers like `int (*fp)(int) = add1;`) also
    // need an `extrn` declaration. Fixture 3643.
    let prototypes: HashSet<&str> = unit
        .functions
        .iter()
        .filter(|f| f.body.is_none())
        .map(|f| f.name.as_str())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered: Vec<String> = Vec::new();
    // Walk global initializers for Ident references to prototype
    // names — those need `extrn _<name>:near` so the linker can
    // resolve the address word.
    for g in &unit.globals {
        if g.is_static || g.is_extern {
            continue;
        }
        let Some(init) = &g.init else { continue };
        let empty_locals: HashSet<String> = HashSet::new();
        walk_idents_for_proto_refs(init, &prototypes, &mut seen, &mut ordered, &empty_locals);
    }
    for f in &unit.functions {
        let Some(body) = &f.body else { continue };
        // Per-function set of locals (params + declared variables).
        // A Call whose name is a local is an indirect call through
        // a function pointer, not an extern reference (fixture 110).
        let mut locals: HashSet<String> = f.params.iter().map(|p| p.name.clone()).collect();
        // Pre-populate with global function-pointer names so the walk
        // skips them the same way it skips locals.
        for g in &global_fnptrs {
            locals.insert((*g).to_string());
        }
        for stmt in body {
            walk_calls(stmt, &defined, &mut locals, &mut seen, &mut ordered);
        }
    }
    ordered
}

/// Walk an init expression for `Ident(name)` references where `name`
/// is a known prototype-only function. Adds to `ordered` in source-
/// encounter order.
fn walk_idents_for_proto_refs(
    e: &crate::ast::Expr,
    prototypes: &std::collections::HashSet<&str>,
    seen: &mut std::collections::HashSet<String>,
    ordered: &mut Vec<String>,
    _locals: &std::collections::HashSet<String>,
) {
    use crate::ast::ExprKind;
    match &e.kind {
        ExprKind::Ident(name) => {
            if prototypes.contains(name.as_str()) && seen.insert(name.clone()) {
                ordered.push(name.clone());
            }
        }
        ExprKind::InitList { items } => {
            for it in items {
                walk_idents_for_proto_refs(it, prototypes, seen, ordered, _locals);
            }
        }
        _ => {}
    }
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
        StmtKind::Goto { .. } | StmtKind::Label { .. } | StmtKind::Empty
        | StmtKind::Asm { .. } => {}
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
        ExprKind::CallVia { addr, args } => {
            // Indirect call through an array/member expression —
            // the callee identity comes from runtime memory, so no
            // EXTRN is added. Just walk into the address and args
            // to pick up any nested direct calls.
            walk_calls_expr(addr, defined, locals, seen, ordered);
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
        ExprKind::AssignLvalueExpr { target, value } => {
            walk_calls_expr(target, defined, locals, seen, ordered);
            walk_calls_expr(value, defined, locals, seen, ordered);
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
        | ExprKind::PseudoReg(_)
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
