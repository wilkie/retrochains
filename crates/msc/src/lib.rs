//! Microsoft C 5.0 compiler reimplementation. Phase 1 covers
//! `int main(void) { return K; }` under `cl /c /AS` for any 16-bit
//! integer literal K. See `specs/plans/MSC_PHASE_1.md` for the
//! sliced roadmap; this file's Slice 1+2 emit the OBJ directly.
//!
//! The reimplementation produces OBJ bytes directly via `crates/obj`
//! rather than going through an ASM-text round-trip (which is BCC's
//! choice because it has a `-S` text output to match). MSC has no
//! equivalent textual intermediate, so the byte-exact target IS the
//! OBJ.

use std::path::Path;

use obj::ObjBuilder;

/// Compile `source_path` (a C source file) to `<NAME>.OBJ` in the
/// current working directory. Mirrors `cl /c /AS HELLO.C`'s file
/// naming: the output basename is the source's basename uppercased
/// with the `.OBJ` extension.
///
/// # Errors
/// Returns [`EmitError`] on I/O failures or unsupported source shapes.
pub fn emit_dash_c(source_path: &Path) -> Result<std::path::PathBuf, EmitError> {
    let source_filename = source_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| EmitError::BadSourcePath(source_path.display().to_string()))?;
    let source = std::fs::read_to_string(source_path).map_err(EmitError::Io)?;
    let ast = parse_main(&source)?;
    let bytes = build_obj(source_filename, &ast);
    let basename = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("OUT");
    let out_name = format!("{}.OBJ", basename.to_ascii_uppercase());
    std::fs::write(&out_name, bytes).map_err(EmitError::Io)?;
    Ok(std::path::PathBuf::from(out_name))
}

/// A minimal AST covering Phase 1's source-shape envelope. Each
/// local is an int at a unique bp-relative slot (`[bp-2]`, `[bp-4]`,
/// …); the return expression is either an int literal or a reference
/// to a local that has a known constant value (MSC folds the latter
/// at parse time — see fixture 4081).
#[derive(Debug, Clone)]
pub struct MainAst {
    /// Initializer per local in source-declaration order. `None` for
    /// uninitialized declarations (`int x;`). Length is the local
    /// count; bytes-of-frame = `locals.len() * 2`.
    pub locals: Vec<Option<i32>>,
    /// The constant-folded value of the `return <expr>;`. The folding
    /// is the parser's responsibility — by the time we reach codegen,
    /// the AST has already resolved `return x;` to its compile-time
    /// constant. Future slices that handle non-constant returns will
    /// widen this variant.
    pub return_value: i32,
}

/// Parse Phase 1's source-shape envelope:
/// ```text
/// int main(void) {
///   [int <name> [= <int>];]*
///   return <int_lit | local_name>;
/// }
/// ```
/// Constant-folds `return x;` when `x` has a single compile-time
/// initializer — matches MSC's own folding for this trivial shape
/// (fixture 4081 confirms).
fn parse_main(source: &str) -> Result<MainAst, EmitError> {
    let body_open = source
        .find('{')
        .ok_or_else(|| EmitError::Unsupported("no `{` in source".to_owned()))?;
    let body_close = source
        .rfind('}')
        .ok_or_else(|| EmitError::Unsupported("no `}` in source".to_owned()))?;
    let body = source[body_open + 1..body_close].trim();

    let mut locals: Vec<(String, Option<i32>)> = Vec::new();
    let mut return_expr: Option<String> = None;

    for stmt in body.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        if let Some(rest) = stmt.strip_prefix("return") {
            return_expr = Some(rest.trim().to_owned());
            continue;
        }
        if let Some(rest) = stmt.strip_prefix("int") {
            let decl = rest.trim();
            if let Some((name, init_expr)) = decl.split_once('=') {
                let init: i32 = init_expr.trim().parse().map_err(|_| {
                    EmitError::Unsupported(format!("non-int initializer `{init_expr}`"))
                })?;
                locals.push((name.trim().to_owned(), Some(init)));
            } else {
                locals.push((decl.to_owned(), None));
            }
            continue;
        }
        return Err(EmitError::Unsupported(format!("statement `{stmt}` not supported")));
    }

    let return_expr = return_expr
        .ok_or_else(|| EmitError::Unsupported("no `return` in body".to_owned()))?;
    let return_value = if let Ok(n) = return_expr.parse::<i32>() {
        n
    } else if let Some((_, Some(init))) =
        locals.iter().find(|(name, _)| *name == return_expr)
    {
        // `return <local>;` where the local has a constant
        // initializer — MSC folds this to the constant. Fixture 4081.
        *init
    } else {
        return Err(EmitError::Unsupported(format!(
            "`return {return_expr};` is not a literal or constant-initialized local",
        )));
    };

    Ok(MainAst {
        locals: locals.into_iter().map(|(_, init)| init).collect(),
        return_value,
    })
}

/// Produce the OBJ bytes for `cl /c /AS <source>` compiling the
/// `int main(void)` shape modeled by `ast`. `source_filename` goes
/// into THEADR uppercased the same way CL does it on the command line.
#[must_use]
pub fn build_obj(source_filename: &str, ast: &MainAst) -> Vec<u8> {
    let mut b = ObjBuilder::new();

    // THEADR — module header. Source filename uppercased.
    b.write_theadr(source_filename);

    // COMENT class 0x00 — translator identification.
    // Payload (after flags+class): "MS C". Flags 0x00 = no-purge,
    // no-list (LINK keeps the COMENT in the output).
    b.write_coment(&[0x00, 0x00, b'M', b'S', b' ', b'C']);

    // COMENT class 0x9F — default library. The linker should pull
    // SLIBCE (small-model, math-emulator C runtime) when resolving
    // unresolved externs at link time. /AS without an explicit
    // /F* flag selects SLIBCE.
    b.write_coment(&[0x00, 0x9F, b'S', b'L', b'I', b'B', b'C', b'E']);

    // COMENT class 0x9D — memory-model marker. MSC's internal tag
    // for the model + a few flag bits. Bytes ASCII "0sO" — three
    // single-byte fields that MSC's LINK reads to verify model
    // consistency across OBJs. We carry the exact bytes /AS emits;
    // characterizing each byte's meaning is Phase 2 work.
    b.write_coment(&[0x00, 0x9D, b'0', b's', b'O']);

    // COMENT class 0xA1 — extension marker. Payload `0x01 "CV"` —
    // probably "CodeView 1" capability hint. Empty-main has no
    // debug info but MSC emits the hint unconditionally under /AS.
    b.write_coment(&[0x00, 0xA1, 0x01, b'C', b'V']);

    // LNAMES — name table. Empty name at index 1 is the standard
    // placeholder; MSC then orders the remaining names with DGROUP
    // first (BCC puts DGROUP last). Indices used by the SEGDEFs
    // and GRPDEF below.
    //   1: ""        (placeholder)
    //   2: DGROUP
    //   3: _TEXT     4: CODE
    //   5: _DATA     6: DATA
    //   7: CONST     (its own class)
    //   8: _BSS      9: BSS
    b.write_lnames(&[
        "",
        "DGROUP",
        "_TEXT", "CODE",
        "_DATA", "DATA",
        "CONST",
        "_BSS", "BSS",
    ]);

    // Build the `_main` body up front so we can stamp its length
    // into the _TEXT SEGDEF. MSC pads odd-length function bodies
    // with a trailing NOP so every function ends on a word boundary.
    let main_body = main_body_for(ast);
    let text_len = u16::try_from(main_body.len()).expect("_TEXT body fits in u16");
    // The chkstk call's displacement bytes live at a fixed offset
    // within the body — same byte position regardless of what comes
    // after. Compute it once for the FIXUPP that patches the call.
    let chkstk_patch_offset = u8::try_from(chkstk_disp_offset(ast))
        .expect("chkstk patch offset fits in u8");

    // SEGDEF table. MSC uses acbp=0x48 (word-aligned, public, big=0,
    // proc=0) for every segment in the small model — distinct from
    // BCC which uses 0x28 (byte-aligned) for _TEXT. The 0x48 value
    // forces TLINK/LINK to pad to a word boundary before each
    // segment, which matters when multiple OBJs combine.
    //
    // SEGDEF #1: _TEXT  — code, sized to `_main`'s padded length
    b.write_segdef16(0x48, text_len, 3, 4, 1);
    // SEGDEF #2: _DATA  — initialized data, 0 bytes (no globals)
    b.write_segdef16(0x48, 0, 5, 6, 1);
    // SEGDEF #3: CONST  — read-only literals, 0 bytes
    b.write_segdef16(0x48, 0, 7, 7, 1);
    // SEGDEF #4: _BSS   — uninitialized data, 0 bytes
    b.write_segdef16(0x48, 0, 8, 9, 1);

    // GRPDEF — DGROUP contains CONST, _BSS, _DATA in *that* order.
    // The order matches MSC's typical link layout: read-only first,
    // then BSS (which links can collapse), then writable. BCC puts
    // _DATA / _BSS in source-declaration order; MSC reorders.
    b.write_grpdef(2, &[3, 4, 2]);

    // FIXUPP — pre-emitted THREAD subrecords. MSC's CL emits these
    // even when only some are referenced; they let later FIXUPs use
    // a 1-byte thread reference instead of the full frame/target
    // datum pair.
    //
    //   Target thread 0 → SEGDEF #3 (CONST)
    //   Target thread 1 → SEGDEF #2 (_DATA)
    //   Target thread 2 → SEGDEF #1 (_TEXT)
    //   Target thread 3 → SEGDEF #4 (_BSS)
    //   Frame  thread 0 → SEGDEF #1 (_TEXT)
    //   Frame  thread 1 → GRPDEF #1 (DGROUP)
    //
    // Each subrecord is (header_byte, index_byte). The header byte
    // encodes D (FIXUP vs THREAD), F (frame vs target), method, and
    // thread number — see specs/formats/OMF.md §FIXUPP THREAD.
    b.write_fixupp(&[
        // Target threads (D=0, F=0, method T0=SEGDEF):
        0x00, 0x03,   // T0: SEGDEF #3 (CONST)
        0x01, 0x02,   // T1: SEGDEF #2 (_DATA)
        0x02, 0x01,   // T2: SEGDEF #1 (_TEXT)
        0x03, 0x04,   // T3: SEGDEF #4 (_BSS)
        // Frame threads (D=0, F=1):
        0x40, 0x01,   // F0: SEGDEF #1 (_TEXT) — method F0=SEGDEF
        0x45, 0x01,   // F1: GRPDEF #1 (DGROUP) — method F1=GRPDEF
    ]);

    // EXTDEF — external symbols. MSC emits three even for an
    // empty-main:
    //   __acrtused  — sentinel that forces LINK to pull in the C
    //                 runtime startup. No FIXUP references it; its
    //                 presence in the EXTDEF table alone does the
    //                 job. Type-idx 0x01 (vs 0x00) marks it as
    //                 special — see Phase 2 for what 0x01 means
    //                 exactly.
    //   __chkstk    — stack-overflow checker; called from every
    //                 function's prologue under /AS.
    //   _main       — declared here as well as defined via PUBDEF
    //                 below. MSC emits the dual declaration for
    //                 the COMDAT / module-level lookup path.
    //
    // EXTDEF entry shape: <name-length><name-bytes><type-idx>.
    // `obj::ObjBuilder::write_extdef` hardcodes type-idx 0, which
    // doesn't fit MSC's `__acrtused: 1` pattern — build the payload
    // manually.
    {
        let mut payload = Vec::new();
        for (name, ty) in [("__acrtused", 0x01u8), ("__chkstk", 0x00), ("_main", 0x00)] {
            payload.push(u8::try_from(name.len()).expect("EXTDEF name fits"));
            payload.extend_from_slice(name.as_bytes());
            payload.push(ty);
        }
        b.write_record(obj::EXTDEF, &payload);
    }

    // PUBDEF — _main at _TEXT offset 0. base_group_idx=0 means the
    // public is relative to its base segment (SEGDEF #1, _TEXT)
    // directly, no group adjustment.
    b.write_pubdef16(0, 1, "_main", 0, 0);

    // COMENT class 0xA2 — link-pass marker. MSC sandwiches the
    // LEDATA records between EXTDEF/PUBDEF setup and the data
    // itself. The payload byte 0x01 is the "start of data" marker;
    // the matching 0xA2 with 0x00 doesn't appear in this OBJ
    // because there's only one LEDATA pass.
    b.write_coment(&[0x00, 0xA2, 0x01]);

    // LEDATA #1 — _TEXT segment, offset 0, `_main` body bytes.
    // See `main_body_for_return` for the shape.
    b.write_ledata16(1, 0, &main_body);

    // FIXUPP — patch the placeholder bytes of the `call __chkstk`.
    //   Locat byte 1 (0x84): bit7=1 (FIXUP), M=0 (self-relative),
    //                        location=0001 (16-bit offset), hi-off=00
    //   Locat byte 2 (<off>): low 8 bits of data-record offset — the
    //                        position of the call's displacement
    //                        within the LEDATA's data bytes. Varies
    //                        with frame size: 3 for 0-byte frames
    //                        (empty-main), 7 with a prologue.
    //   Fix Data  (0x56):    F=0 (frame explicit), frame-method=F5
    //                        (target's segment), T=0 (target explicit),
    //                        P=1 (no displacement), target-method=T2
    //                        (EXTDEF)
    //   Target datum (0x02): EXTDEF index 2 (__chkstk)
    b.write_fixupp(&[0x84, chkstk_patch_offset, 0x56, 0x02]);

    // MODEND — end of module. No-entry form (the executable's entry
    // point comes from the PUBDEF of `_main` resolved at link time,
    // not from MODEND's start-address field).
    b.write_modend16_no_entry();

    b.into_bytes()
}

/// MSC's `_main` body for `int main(void) { <locals + return> }`.
/// Shape depends on whether the function has a stack frame:
///
/// **Zero locals (fixtures 4075 / 4076 / 4077 / 4078):**
/// ```text
/// 33 c0           xor ax, ax       ; chkstk arg = 0
/// e8 00 00        call __chkstk   ; FIXUP'd at offset 3
/// <return load>   (see below)
/// c3              ret
/// [90]            nop pad if odd
/// ```
/// No prologue or epilogue — MSC elides them entirely for a 0-byte
/// frame.
///
/// **N≥1 locals (fixtures 4079 / 4080 / 4081):**
/// ```text
/// 55              push bp
/// 8b ec           mov bp, sp
/// b8 <2N> 00      mov ax, frame_bytes  ; chkstk arg
/// e8 00 00        call __chkstk        ; FIXUP'd at offset 7
/// <initializers>  c7 46 <disp> <lo> <hi>   ; per initialized local
/// <return load>
/// 8b e5           mov sp, bp
/// 5d              pop bp
/// c3              ret
/// [90]            nop pad if odd
/// ```
///
/// **Return-value load** picks between two encodings:
/// - `return 0;` (fixture 4075, 4079, 4080): `2b c0` (sub ax, ax).
/// - any other literal: `b8 <lo> <hi>` (mov ax, imm16).
///
/// The "sub ax, ax for 0" idiom is MSC's special-case — it doesn't
/// re-use the existing 0 in AX from the chkstk arg even when it
/// could; the codegen always emits the explicit return-value
/// instruction.
fn main_body_for(ast: &MainAst) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    let frame_bytes = ast.locals.len() * 2;

    if frame_bytes > 0 {
        // Prologue: standard 8086 frame setup. MSC doesn't use the
        // 186-era ENTER instruction under /AS even when the model
        // would allow it.
        body.extend_from_slice(&[0x55, 0x8B, 0xEC]);
        // Chkstk arg as `mov ax, <frame_bytes>`. The 16-bit immediate
        // form is always picked here even for sizes that would fit
        // in a `push imm8 / pop ax`.
        body.push(0xB8);
        body.extend_from_slice(&u16::try_from(frame_bytes)
            .expect("frame fits in u16")
            .to_le_bytes());
    } else {
        // No prologue. `xor ax, ax` doubles as the chkstk arg of 0;
        // the eventual return-0 path emits its own `sub ax, ax` to
        // re-zero AX (chkstk clobbers it).
        body.extend_from_slice(&[0x33, 0xC0]);
    }
    // `call __chkstk` with the 2-byte displacement left as zeros for
    // the FIXUP to patch.
    body.extend_from_slice(&[0xE8, 0x00, 0x00]);

    // Initialized-local writes — each `int x = K;` becomes
    // `mov word ptr [bp-disp], K` (`c7 46 <disp> <lo> <hi>`).
    // Locals are laid out at `[bp-2]`, `[bp-4]`, … in source order
    // (same as BCC). Uninitialized declarations get no write.
    for (i, init) in ast.locals.iter().enumerate() {
        if let Some(value) = init {
            let disp = -(i16::try_from(i + 1).expect("local index fits") * 2);
            let imm = (*value as u32 & 0xFFFF) as u16;
            body.push(0xC7);
            body.push(0x46);
            body.push(disp as u8);
            body.extend_from_slice(&imm.to_le_bytes());
        }
    }

    // Return-value load.
    if ast.return_value == 0 {
        body.extend_from_slice(&[0x2B, 0xC0]);
    } else {
        let imm = (ast.return_value as u32 & 0xFFFF) as u16;
        body.push(0xB8);
        body.extend_from_slice(&imm.to_le_bytes());
    }

    // Epilogue + ret. Same conditional as the prologue.
    if frame_bytes > 0 {
        body.extend_from_slice(&[0x8B, 0xE5, 0x5D]);
    }
    body.push(0xC3);

    // Pad to an even byte count with a single NOP. MSC enforces this
    // unconditionally — every `_main` body in fixtures 4075–4081
    // ends on a word boundary, with the pad NOP appended when the
    // natural shape was odd.
    if body.len() % 2 != 0 {
        body.push(0x90);
    }
    body
}

/// Byte offset within `_main`'s body where the `e8 disp16` call to
/// `__chkstk` keeps its displacement bytes — the location the
/// FIXUPP patches at link time. With no prologue: bytes 3-4 (offset
/// 3, after `33 c0 e8`). With a prologue: bytes 7-8 (offset 7, after
/// `55 8b ec b8 <lo> <hi> e8`).
fn chkstk_disp_offset(ast: &MainAst) -> usize {
    if ast.locals.is_empty() { 3 } else { 7 }
}

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("could not read source filename from path {0:?}")]
    BadSourcePath(String),
    #[error("unsupported source shape: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
