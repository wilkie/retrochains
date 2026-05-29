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

/// A minimal AST covering Phase 1's source-shape envelope.
#[derive(Debug, Clone)]
pub struct MainAst {
    /// Initializer per local in source-declaration order. `None` for
    /// uninitialized declarations (`int x;`). Length is the local
    /// count; bytes-of-frame = `locals.len() * 2`.
    pub locals: Vec<Option<i32>>,
    /// The `return <expr>;` expression. Codegen folds constant
    /// sub-expressions at emit time and routes non-folded ones
    /// through AX with a small per-operator helper. Slice 4
    /// fixtures (4084-4089) cover the patterns currently supported.
    pub return_expr: Expr,
}

/// Expression AST. Phase 1 grows this incrementally as fixtures
/// land — Slice 3 had `IntLit` and `Local`; Slice 4 adds `BinOp`.
#[derive(Debug, Clone)]
pub enum Expr {
    /// A 16-bit-truncated int literal.
    IntLit(i32),
    /// Reference to a local by index into `MainAst.locals`.
    Local(usize),
    /// A binary operation. `op` selects add/sub/mul/...; codegen
    /// picks the actual instruction (inc/dec/shl/shift-add/imul)
    /// based on the operands.
    BinOp { op: BinOp, left: Box<Expr>, right: Box<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
}

impl Expr {
    /// Try to fold the expression to a compile-time integer.
    /// Returns `Some(K)` when every operand is itself foldable —
    /// either a literal, or a local with a constant initializer
    /// (fixture 4081 confirms MSC folds `return x;` for such
    /// locals). Used by codegen to pick between `mov ax, K` and
    /// the runtime arithmetic path.
    fn fold(&self, locals: &[Option<i32>]) -> Option<i32> {
        match self {
            Expr::IntLit(k) => Some(*k),
            Expr::Local(i) => locals.get(*i).copied().flatten(),
            Expr::BinOp { op, left, right } => {
                let l = left.fold(locals)?;
                let r = right.fold(locals)?;
                Some(match op {
                    BinOp::Add => l.wrapping_add(r),
                    BinOp::Sub => l.wrapping_sub(r),
                    BinOp::Mul => l.wrapping_mul(r),
                })
            }
        }
    }
}

/// Parse Phase 1's source-shape envelope:
/// ```text
/// int main(void) {
///   [int <name> [= <int>];]*
///   return <expr>;
/// }
/// ```
/// where `<expr>` is a literal, a local name, or a single binary
/// operator (`+`, `-`, `*`) between two leaf operands. Nested
/// binops and parens come with a later slice.
fn parse_main(source: &str) -> Result<MainAst, EmitError> {
    let body_open = source
        .find('{')
        .ok_or_else(|| EmitError::Unsupported("no `{` in source".to_owned()))?;
    let body_close = source
        .rfind('}')
        .ok_or_else(|| EmitError::Unsupported("no `}` in source".to_owned()))?;
    let body = source[body_open + 1..body_close].trim();

    let mut local_names: Vec<String> = Vec::new();
    let mut local_inits: Vec<Option<i32>> = Vec::new();
    let mut return_expr_text: Option<String> = None;

    for stmt in body.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        if let Some(rest) = stmt.strip_prefix("return") {
            return_expr_text = Some(rest.trim().to_owned());
            continue;
        }
        if let Some(rest) = stmt.strip_prefix("int") {
            let decl = rest.trim();
            if let Some((name, init_expr)) = decl.split_once('=') {
                let init: i32 = init_expr.trim().parse().map_err(|_| {
                    EmitError::Unsupported(format!("non-int initializer `{init_expr}`"))
                })?;
                local_names.push(name.trim().to_owned());
                local_inits.push(Some(init));
            } else {
                local_names.push(decl.to_owned());
                local_inits.push(None);
            }
            continue;
        }
        return Err(EmitError::Unsupported(format!("statement `{stmt}` not supported")));
    }

    let return_text = return_expr_text
        .ok_or_else(|| EmitError::Unsupported("no `return` in body".to_owned()))?;
    let return_expr = parse_expr(&return_text, &local_names)?;

    Ok(MainAst { locals: local_inits, return_expr })
}

/// Parse one of the expression shapes Slice 4 supports:
/// - `<int>`
/// - `<local-name>`
/// - `<atom> + <atom>`, `<atom> - <atom>`, `<atom> * <atom>`
///   (single binop, atoms are themselves literals or locals)
fn parse_expr(text: &str, local_names: &[String]) -> Result<Expr, EmitError> {
    let text = text.trim();
    // Look for a top-level operator. Phase 1's grammar is too
    // restricted for precedence to matter — at most one operator.
    for op_ch in ['+', '-', '*'] {
        if let Some(i) = text.find(op_ch) {
            // Exclude unary `-` at position 0 so `-1` stays a literal.
            if op_ch == '-' && i == 0 {
                continue;
            }
            let (left, right_with_op) = text.split_at(i);
            let right = &right_with_op[1..];
            let l = parse_atom(left.trim(), local_names)?;
            let r = parse_atom(right.trim(), local_names)?;
            let op = match op_ch {
                '+' => BinOp::Add,
                '-' => BinOp::Sub,
                '*' => BinOp::Mul,
                _ => unreachable!(),
            };
            return Ok(Expr::BinOp { op, left: Box::new(l), right: Box::new(r) });
        }
    }
    parse_atom(text, local_names)
}

fn parse_atom(text: &str, local_names: &[String]) -> Result<Expr, EmitError> {
    let text = text.trim();
    if let Ok(n) = text.parse::<i32>() {
        return Ok(Expr::IntLit(n));
    }
    if let Some(idx) = local_names.iter().position(|n| n == text) {
        return Ok(Expr::Local(idx));
    }
    Err(EmitError::Unsupported(format!("atom `{text}` not a literal or known local")))
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

    // Return-value load. Folded constants take the literal path
    // (`sub ax, ax` for 0, `mov ax, imm16` otherwise); non-folded
    // expressions route through `emit_expr_to_ax`.
    if let Some(k) = ast.return_expr.fold(&ast.locals) {
        if k == 0 {
            body.extend_from_slice(&[0x2B, 0xC0]);
        } else {
            let imm = (k as u32 & 0xFFFF) as u16;
            body.push(0xB8);
            body.extend_from_slice(&imm.to_le_bytes());
        }
    } else {
        emit_expr_to_ax(&ast.return_expr, &ast.locals, &mut body);
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

/// Append the bytes that compute `expr` into AX. Caller has already
/// emitted the prologue + chkstk call; what we emit here lives
/// between the chkstk call and the return-path epilogue. Phase 1
/// Slice 4 supports a tight set of patterns — every other shape
/// panics with a clear message so the missing case is obvious when
/// a future fixture hits it.
fn emit_expr_to_ax(expr: &Expr, locals: &[Option<i32>], out: &mut Vec<u8>) {
    match expr {
        Expr::IntLit(k) => {
            // Foldable path is handled by the caller; this arm only
            // fires if the caller bypassed folding.
            let imm = (*k as u32 & 0xFFFF) as u16;
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Expr::Local(i) => {
            emit_load_local(*i, out);
        }
        Expr::BinOp { op, left, right } => {
            emit_binop(*op, left, right, locals, out);
        }
    }
}

/// `mov ax, word ptr [bp-disp]` — 3-byte form `8B 46 disp8`. Used
/// for all local loads in Phase 1; only -2, -4, -6 displacements
/// are exercised today.
fn emit_load_local(idx: usize, out: &mut Vec<u8>) {
    let disp = -(i16::try_from(idx + 1).expect("local index fits") * 2);
    out.push(0x8B);
    out.push(0x46);
    out.push(disp as u8);
}

fn emit_binop(op: BinOp, left: &Expr, right: &Expr, locals: &[Option<i32>], out: &mut Vec<u8>) {
    // Pattern: <Local> <op> <IntLit>. The very small set of (op, K)
    // shapes we recognize:
    //   Add, K=1   → inc ax
    //   Sub, K=1   → dec ax
    //   Mul, K=2   → shl ax, 1
    //   Mul, K=3   → mov cx, ax; shl ax, 1; add ax, cx  (shift+add)
    //   Add, K=any → add ax, K   (other K's TBD by a future fixture)
    //   Sub, K=any → sub ax, K
    if let (Expr::Local(li), Expr::IntLit(k)) = (left, right) {
        emit_load_local(*li, out);
        emit_imm_op(op, *k, out);
        return;
    }
    // Pattern: <Local> <op> <Local> — `add ax, [bp-disp]` family.
    // Fixture 4086 confirms this shape for Add; the Sub mirror is
    // expected from the symmetry but isn't fixtured yet.
    if let (Expr::Local(li), Expr::Local(ri)) = (left, right) {
        emit_load_local(*li, out);
        emit_mem_op(op, *ri, out);
        return;
    }
    // Pattern with a foldable side — recurse with the folded literal
    // in place. Lets `(2+x)` collapse to `(<lit> + <local>)` etc.
    if let Some(k) = left.fold(locals) {
        emit_binop(op, &Expr::IntLit(k), right, locals, out);
        return;
    }
    if let Some(k) = right.fold(locals) {
        emit_binop(op, left, &Expr::IntLit(k), locals, out);
        return;
    }
    panic!("Slice 4 binop shape not yet supported: {op:?} of {left:?}, {right:?}");
}

/// Per-operator emit for `<reg-AX> <op> <imm>`. Picks the smallest
/// MSC-equivalent form (single-byte inc/dec, shl, shift-and-add)
/// before falling back to the generic `add/sub ax, imm16`.
fn emit_imm_op(op: BinOp, k: i32, out: &mut Vec<u8>) {
    let k16 = (k as u32 & 0xFFFF) as u16;
    match (op, k16) {
        (BinOp::Add, 1) => out.push(0x40),                  // inc ax
        (BinOp::Sub, 1) => out.push(0x48),                  // dec ax
        (BinOp::Mul, 2) => out.extend_from_slice(&[0xD1, 0xE0]), // shl ax, 1
        // `x * 3` → `mov cx, ax; shl ax, 1; add ax, cx`. Fixture 4088.
        // MSC picks this 6-byte shift-and-add over `imul ax, 3` for
        // single-use *3.
        (BinOp::Mul, 3) => out.extend_from_slice(&[
            0x8B, 0xC8,         // mov cx, ax
            0xD1, 0xE0,         // shl ax, 1
            0x03, 0xC1,         // add ax, cx
        ]),
        // Generic `add/sub ax, imm16` — Phase 2 fixtures will pin
        // down whether MSC ever picks `inc / dec` for K = 2 (BCC
        // does for some shapes; MSC unknown).
        (BinOp::Add, _) => {
            out.push(0x05);                                 // add ax, imm16
            out.extend_from_slice(&k16.to_le_bytes());
        }
        (BinOp::Sub, _) => {
            out.push(0x2D);                                 // sub ax, imm16
            out.extend_from_slice(&k16.to_le_bytes());
        }
        (BinOp::Mul, _) => {
            panic!("Slice 4 multiplication by {k} not yet covered by a fixture");
        }
    }
}

/// Per-operator emit for `<reg-AX> <op> word ptr [bp-disp]`. The
/// opcode-prefix byte for memory-source forms: 03=ADD, 2B=SUB. Mul
/// from memory isn't a single-instruction shape so it's not handled
/// here.
fn emit_mem_op(op: BinOp, local_idx: usize, out: &mut Vec<u8>) {
    let disp = -(i16::try_from(local_idx + 1).expect("local index fits") * 2);
    let opcode = match op {
        BinOp::Add => 0x03,
        BinOp::Sub => 0x2B,
        BinOp::Mul => panic!("Slice 4 doesn't handle `<local> * <local>` (no fixture)"),
    };
    out.push(opcode);
    out.push(0x46);  // ModR/M: mod=01 (disp8), reg=000 (AX), r/m=110 (BP-rel)
    out.push(disp as u8);
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
