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
    /// Statements after the local declarations, in source order.
    /// Always ends in a `Return` or an `If` whose every leaf is a
    /// `Return` — Phase 1 functions don't have an implicit
    /// fall-through return path.
    pub body: Vec<Stmt>,
}

/// Statement AST. Phase 1 covers `return <expr>;`,
/// `if (<cond>) <stmt>;`, `if (<cond>) <stmt> else <stmt>;`,
/// `while (<cond>) <stmt>;`, and `<local> = <expr>;`. Block
/// statements (`{ ... }`) come with the multi-line bodies in a
/// later slice.
#[derive(Debug, Clone)]
pub enum Stmt {
    Return(Expr),
    /// Empty statement (`;`). Carries no codegen. Used as the body
    /// of an empty for-loop, fixture 4097.
    Empty,
    If {
        cond: Cond,
        then_branch: Box<Stmt>,
        else_branch: Option<Box<Stmt>>,
    },
    While {
        cond: Cond,
        body: Box<Stmt>,
    },
    /// `do <body> while (<cond>);` — body runs first, cond checked
    /// after. MSC's peephole: when the body's last instruction
    /// already sets ZF for the cond (e.g. body is `x = x - 1;` and
    /// cond is `x`), MSC drops the explicit cmp and chains the jcc
    /// off the body's flags. Fixture 4098.
    DoWhile {
        body: Box<Stmt>,
        cond: Cond,
    },
    /// `for (<init>; <cond>; <step>) <body>;` — modeled as its own
    /// variant rather than desugared to `init; while (cond) {
    /// body; step; }` because MSC's emitted layout interleaves
    /// step before body inside the loop section (fixture 4097).
    For {
        init: Box<Stmt>,
        cond: Cond,
        step: Box<Stmt>,
        body: Box<Stmt>,
    },
    Assign {
        local_idx: usize,
        value: Expr,
    },
}

/// Condition for `if` (and later `while`/`for`). Slice 5 covers the
/// truthiness test (`if (x)`) and equality compare (`if (x == K)`);
/// other relational operators come with future fixtures.
#[derive(Debug, Clone)]
pub enum Cond {
    /// `if (<expr>)` — non-zero is truthy. MSC lowers to
    /// `cmp <expr>, 0; je skip-body`.
    Truthy(Expr),
    /// `if (<left> <op> <right>)` — comparison.
    Cmp { op: RelOp, left: Expr, right: Expr },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelOp {
    Eq,
    Ne,
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

/// A token used by the small recursive-descent parser. Phase 1's
/// source is tight enough that we only need keywords + ident +
/// integer + a handful of punctuation tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Kw(&'static str),
    Ident(String),
    Int(i32),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Semi,
    Assign,
    EqEq,
    NotEq,
    Plus,
    Minus,
    Star,
}

fn tokenize(source: &str) -> Result<Vec<Tok>, EmitError> {
    let bytes = source.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'(' => { toks.push(Tok::LParen); i += 1; }
            b')' => { toks.push(Tok::RParen); i += 1; }
            b'{' => { toks.push(Tok::LBrace); i += 1; }
            b'}' => { toks.push(Tok::RBrace); i += 1; }
            b';' => { toks.push(Tok::Semi); i += 1; }
            b'+' => { toks.push(Tok::Plus); i += 1; }
            b'*' => { toks.push(Tok::Star); i += 1; }
            b'=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::EqEq);
                    i += 2;
                } else {
                    toks.push(Tok::Assign);
                    i += 1;
                }
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::NotEq);
                    i += 2;
                } else {
                    return Err(EmitError::Unsupported(
                        "bare `!` (logical not) not yet supported".to_owned(),
                    ));
                }
            }
            b'-' => { toks.push(Tok::Minus); i += 1; }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let text = std::str::from_utf8(&bytes[start..i])
                    .map_err(|_| EmitError::Unsupported("non-ASCII in integer".to_owned()))?;
                let n: i32 = text
                    .parse()
                    .map_err(|_| EmitError::Unsupported(format!("bad integer `{text}`")))?;
                toks.push(Tok::Int(n));
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let text = std::str::from_utf8(&bytes[start..i])
                    .map_err(|_| EmitError::Unsupported("non-ASCII in identifier".to_owned()))?;
                let tok = match text {
                    "int" => Tok::Kw("int"),
                    "main" => Tok::Kw("main"),
                    "void" => Tok::Kw("void"),
                    "return" => Tok::Kw("return"),
                    "if" => Tok::Kw("if"),
                    "else" => Tok::Kw("else"),
                    "while" => Tok::Kw("while"),
                    "do" => Tok::Kw("do"),
                    "for" => Tok::Kw("for"),
                    _ => Tok::Ident(text.to_owned()),
                };
                toks.push(tok);
            }
            _ => {
                return Err(EmitError::Unsupported(format!(
                    "unexpected character `{}` in source",
                    c as char
                )));
            }
        }
    }
    Ok(toks)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    local_names: Vec<String>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn eat(&mut self, want: &Tok) -> Result<(), EmitError> {
        if self.peek() == Some(want) {
            self.pos += 1;
            Ok(())
        } else {
            Err(EmitError::Unsupported(format!(
                "expected {want:?}, got {:?}",
                self.peek()
            )))
        }
    }
    fn bump(&mut self) -> Option<&Tok> {
        let t = self.toks.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
}

/// Parse Phase 1's source-shape envelope:
/// ```text
/// int main(void) {
///   [int <name> [= <int>];]*
///   <stmt>+
/// }
/// ```
/// Statements are `return <expr>;` or
/// `if (<cond>) <stmt> [else <stmt>]`. Cond is `<expr>` or
/// `<expr> == <expr>`. Expressions are the Slice-4 shapes (literal,
/// local, single binop).
fn parse_main(source: &str) -> Result<MainAst, EmitError> {
    let toks = tokenize(source)?;
    let mut p = Parser { toks: &toks, pos: 0, local_names: Vec::new() };

    // `int main(void) {`
    p.eat(&Tok::Kw("int"))?;
    p.eat(&Tok::Kw("main"))?;
    p.eat(&Tok::LParen)?;
    p.eat(&Tok::Kw("void"))?;
    p.eat(&Tok::RParen)?;
    p.eat(&Tok::LBrace)?;

    // `int <name> [= <int>];` declarations.
    let mut local_inits: Vec<Option<i32>> = Vec::new();
    while matches!(p.peek(), Some(Tok::Kw("int"))) {
        p.bump(); // int
        let name = match p.bump() {
            Some(Tok::Ident(s)) => s.clone(),
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected identifier in declaration, got {other:?}"
                )));
            }
        };
        let init = if matches!(p.peek(), Some(Tok::Assign)) {
            p.bump();
            Some(parse_signed_int(&mut p)?)
        } else {
            None
        };
        p.eat(&Tok::Semi)?;
        p.local_names.push(name);
        local_inits.push(init);
    }

    // Body statements until the closing `}`.
    let mut body = Vec::new();
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        body.push(parse_stmt(&mut p)?);
    }
    p.eat(&Tok::RBrace)?;

    Ok(MainAst { locals: local_inits, body })
}

fn parse_signed_int(p: &mut Parser<'_>) -> Result<i32, EmitError> {
    let sign = if matches!(p.peek(), Some(Tok::Minus)) {
        p.bump();
        -1
    } else {
        1
    };
    match p.bump() {
        Some(Tok::Int(n)) => Ok(sign * n),
        other => Err(EmitError::Unsupported(format!(
            "expected integer literal, got {other:?}"
        ))),
    }
}

fn parse_stmt(p: &mut Parser<'_>) -> Result<Stmt, EmitError> {
    match p.peek() {
        Some(Tok::Kw("return")) => {
            p.bump();
            let expr = parse_expr(p)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Return(expr))
        }
        Some(Tok::Kw("if")) => {
            p.bump();
            p.eat(&Tok::LParen)?;
            let cond = parse_cond(p)?;
            p.eat(&Tok::RParen)?;
            let then_branch = Box::new(parse_stmt(p)?);
            let else_branch = if matches!(p.peek(), Some(Tok::Kw("else"))) {
                p.bump();
                Some(Box::new(parse_stmt(p)?))
            } else {
                None
            };
            Ok(Stmt::If { cond, then_branch, else_branch })
        }
        Some(Tok::Kw("while")) => {
            p.bump();
            p.eat(&Tok::LParen)?;
            let cond = parse_cond(p)?;
            p.eat(&Tok::RParen)?;
            let body = Box::new(parse_stmt(p)?);
            Ok(Stmt::While { cond, body })
        }
        Some(Tok::Kw("do")) => {
            p.bump();
            let body = Box::new(parse_stmt(p)?);
            p.eat(&Tok::Kw("while"))?;
            p.eat(&Tok::LParen)?;
            let cond = parse_cond(p)?;
            p.eat(&Tok::RParen)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::DoWhile { body, cond })
        }
        Some(Tok::Kw("for")) => {
            p.bump();
            p.eat(&Tok::LParen)?;
            // Init is an assignment expression-statement without a
            // trailing semi (the semi is the for-syntax separator).
            let init = Box::new(parse_assign_no_semi(p)?);
            p.eat(&Tok::Semi)?;
            let cond = parse_cond(p)?;
            p.eat(&Tok::Semi)?;
            let step = Box::new(parse_assign_no_semi(p)?);
            p.eat(&Tok::RParen)?;
            let body = Box::new(parse_stmt(p)?);
            Ok(Stmt::For { init, cond, step, body })
        }
        Some(Tok::Semi) => {
            p.bump();
            Ok(Stmt::Empty)
        }
        Some(Tok::Ident(_)) => {
            // Assignment statement `<local> = <expr>;`.
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                _ => unreachable!(),
            };
            let local_idx = p.local_names
                .iter()
                .position(|n| *n == name)
                .ok_or_else(|| {
                    EmitError::Unsupported(format!("assignment to unknown local `{name}`"))
                })?;
            p.eat(&Tok::Assign)?;
            let value = parse_expr(p)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Assign { local_idx, value })
        }
        other => Err(EmitError::Unsupported(format!(
            "statement starting with {other:?} not yet supported"
        ))),
    }
}

/// Parse `<local> = <expr>` (no trailing `;`) — used inside
/// for-clauses where the semis are the for-syntax separators, not
/// statement terminators.
fn parse_assign_no_semi(p: &mut Parser<'_>) -> Result<Stmt, EmitError> {
    let name = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected identifier in assignment, got {other:?}"
            )));
        }
    };
    let local_idx = p
        .local_names
        .iter()
        .position(|n| *n == name)
        .ok_or_else(|| EmitError::Unsupported(format!("unknown local `{name}` in for-clause")))?;
    p.eat(&Tok::Assign)?;
    let value = parse_expr(p)?;
    Ok(Stmt::Assign { local_idx, value })
}

fn parse_cond(p: &mut Parser<'_>) -> Result<Cond, EmitError> {
    let left = parse_expr(p)?;
    let op = match p.peek() {
        Some(Tok::EqEq) => Some(RelOp::Eq),
        Some(Tok::NotEq) => Some(RelOp::Ne),
        _ => None,
    };
    if let Some(op) = op {
        p.bump();
        let right = parse_expr(p)?;
        return Ok(Cond::Cmp { op, left, right });
    }
    Ok(Cond::Truthy(left))
}

/// Expression parser — recognizes the Slice-4 shapes:
/// `<atom>` or `<atom> <op> <atom>` where op is `+ - *`.
fn parse_expr(p: &mut Parser<'_>) -> Result<Expr, EmitError> {
    let left = parse_atom(p)?;
    let op = match p.peek() {
        Some(Tok::Plus) => Some(BinOp::Add),
        Some(Tok::Minus) => Some(BinOp::Sub),
        Some(Tok::Star) => Some(BinOp::Mul),
        _ => None,
    };
    if let Some(op) = op {
        p.bump();
        let right = parse_atom(p)?;
        return Ok(Expr::BinOp { op, left: Box::new(left), right: Box::new(right) });
    }
    Ok(left)
}

fn parse_atom(p: &mut Parser<'_>) -> Result<Expr, EmitError> {
    let tok = p.bump().cloned();
    match tok {
        Some(Tok::Int(n)) => Ok(Expr::IntLit(n)),
        Some(Tok::Minus) => match p.bump().cloned() {
            Some(Tok::Int(n)) => Ok(Expr::IntLit(-n)),
            other => Err(EmitError::Unsupported(format!(
                "expected int after unary -, got {other:?}"
            ))),
        },
        Some(Tok::Ident(name)) => {
            if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                Ok(Expr::Local(idx))
            } else {
                Err(EmitError::Unsupported(format!("unknown identifier `{name}`")))
            }
        }
        other => Err(EmitError::Unsupported(format!(
            "expected atom, got {other:?}"
        ))),
    }
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

    // Emit each top-level statement. Every leaf is a `Return`, and
    // each Return contributes its own full epilogue (`mov sp,bp;
    // pop bp; ret` — or just `ret` for 0-byte frames) — MSC does
    // not merge epilogues across branches. Fixture 4092 confirms.
    //
    // Track reachability so an unreachable trailing statement
    // (after `if (1) return …;`, fixture 4095) is dropped — MSC
    // does this aggressively, parallel to its constant-condition
    // elision.
    let has_frame = frame_bytes > 0;
    let mut reachable = true;
    for stmt in &ast.body {
        if !reachable {
            break;
        }
        emit_stmt(stmt, &ast.locals, has_frame, &mut body);
        if stmt_always_returns(stmt, &ast.locals) {
            reachable = false;
        }
    }

    // Pad to an even byte count with a single NOP. MSC enforces this
    // unconditionally — every `_main` body in fixtures 4075–4083
    // ends on a word boundary, with the pad NOP appended when the
    // natural shape was odd.
    if body.len() % 2 != 0 {
        body.push(0x90);
    }
    body
}

/// Emit a single statement (recursive: if-statements contain
/// nested statements). Returns no value — appends directly to `out`.
fn emit_stmt(stmt: &Stmt, locals: &[Option<i32>], has_frame: bool, out: &mut Vec<u8>) {
    match stmt {
        Stmt::Return(expr) => emit_return(expr, locals, has_frame, out),
        Stmt::Empty => {}
        Stmt::Assign { local_idx, value } => emit_assign(*local_idx, value, locals, out),
        Stmt::While { cond, body } => emit_while(cond, body, locals, has_frame, out),
        Stmt::DoWhile { body, cond } => emit_do_while(body, cond, locals, has_frame, out),
        Stmt::For { init, cond, step, body } => {
            emit_for(init, cond, step, body, locals, has_frame, out);
        }
        Stmt::If { cond, then_branch, else_branch } => {
            // Constant-condition elision: when the cond folds to a
            // compile-time integer, MSC keeps only the live branch
            // and drops the comparison + jump entirely. Fixtures
            // 4094 (if (0)) and 4095 (if (1)) confirm.
            if let Some(k) = fold_cond(cond, locals) {
                if k != 0 {
                    emit_stmt(then_branch, locals, has_frame, out);
                } else if let Some(else_branch) = else_branch {
                    emit_stmt(else_branch, locals, has_frame, out);
                }
                return;
            }
            // Build the then-branch into a scratch buffer so we know
            // its byte count for the conditional-jump displacement.
            let mut then_buf = Vec::new();
            emit_stmt(then_branch, locals, has_frame, &mut then_buf);
            let then_len = then_buf.len();
            // MSC's `if (<cond>) <body>` lowers to a skip-then jump:
            // emit the inverted predicate as `cmp X, Y; <jcc> skip;`
            // where the jcc displacement is the byte count of the
            // then-body (+ any else-body skip-over).
            let take_then_disp = i8::try_from(then_len)
                .expect("then-body short enough for jcc rel8");
            emit_cond_skip(cond, take_then_disp, out);
            out.extend_from_slice(&then_buf);
            if let Some(else_branch) = else_branch {
                emit_stmt(else_branch, locals, has_frame, out);
            }
        }
    }
}

/// True when `stmt` unconditionally returns — so a following
/// statement at the same nesting level is unreachable. Used to
/// drop trailing dead code (fixture 4095: `if (1) return 1; return
/// 0;` keeps only the `return 1;` path).
fn stmt_always_returns(stmt: &Stmt, locals: &[Option<i32>]) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::Empty => false,
        Stmt::Assign { .. } => false,
        // Loops with a runtime cond can fall through; the
        // const-true infinite-loop case isn't exercised yet so we
        // conservatively answer false.
        Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } => false,
        Stmt::If { cond, then_branch, else_branch } => {
            if let Some(k) = fold_cond(cond, locals) {
                if k != 0 {
                    // Live branch is the then-branch.
                    stmt_always_returns(then_branch, locals)
                } else if let Some(eb) = else_branch {
                    stmt_always_returns(eb, locals)
                } else {
                    false
                }
            } else {
                // Runtime cond: every branch must always return.
                stmt_always_returns(then_branch, locals)
                    && else_branch
                        .as_ref()
                        .is_some_and(|eb| stmt_always_returns(eb, locals))
            }
        }
    }
}

/// Try to fold the condition to a compile-time boolean (returned as
/// an int: 0 = false, anything else = true). Mirrors MSC's
/// const-condition elision. Fixtures 4094 / 4095.
fn fold_cond(cond: &Cond, locals: &[Option<i32>]) -> Option<i32> {
    match cond {
        Cond::Truthy(e) => e.fold(locals),
        Cond::Cmp { op, left, right } => {
            let l = left.fold(locals)?;
            let r = right.fold(locals)?;
            Some(match op {
                RelOp::Eq => i32::from(l == r),
                RelOp::Ne => i32::from(l != r),
            })
        }
    }
}

fn emit_return(expr: &Expr, locals: &[Option<i32>], has_frame: bool, out: &mut Vec<u8>) {
    // Return-value load: foldable expressions take the literal path
    // (`sub ax, ax` for 0, `mov ax, imm16` otherwise); non-folded
    // expressions route through `emit_expr_to_ax`.
    if let Some(k) = expr.fold(locals) {
        if k == 0 {
            out.extend_from_slice(&[0x2B, 0xC0]);
        } else {
            let imm = (k as u32 & 0xFFFF) as u16;
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
    } else {
        emit_expr_to_ax(expr, locals, out);
    }
    // Per-return epilogue. 0-byte frames skip both the prologue and
    // epilogue (fixture 4075); frame-using functions restore SP and
    // pop BP before the ret (fixtures 4079+). Every Return statement
    // contributes its own copy — no merge across branches.
    if has_frame {
        out.extend_from_slice(&[0x8B, 0xE5, 0x5D]);
    }
    out.push(0xC3);
}

/// `<local> = <expr>;`. Phase 1 supports the peephole
/// `<local> = <same-local> ± 1;` → `inc/dec word ptr [bp-disp]`
/// (fixture 4096: `x = x - 1;` in a while body). The general path
/// — `mov ax, <expr>; mov [bp-disp], ax` — is reserved for a
/// future fixture that exercises a non-peephole shape.
fn emit_assign(local_idx: usize, value: &Expr, locals: &[Option<i32>], out: &mut Vec<u8>) {
    let disp = -(i16::try_from(local_idx + 1).expect("local index fits") * 2);
    // Peephole: `x = x + 1;` and `x = x - 1;` become in-place
    // `inc/dec word ptr [bp-disp]` (3-byte `FF /0 r/m` for inc,
    // `FF /1 r/m` for dec).
    if let Expr::BinOp { op, left, right } = value
        && let Expr::Local(li) = left.as_ref()
        && *li == local_idx
        && let Expr::IntLit(1) = right.as_ref()
    {
        match op {
            BinOp::Add => {
                out.push(0xFF);
                out.push(0x46);              // ModR/M mod=01 reg=000 (/0=INC) r/m=110
                out.push(disp as u8);
                return;
            }
            BinOp::Sub => {
                out.push(0xFF);
                out.push(0x4E);              // ModR/M mod=01 reg=001 (/1=DEC) r/m=110
                out.push(disp as u8);
                return;
            }
            _ => {}
        }
    }
    // General path: evaluate the RHS into AX, then store.
    if let Some(k) = value.fold(locals) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x46);
        out.push(disp as u8);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out);
        out.push(0x89);                       // MOV r/m16, r16  (AX → mem)
        out.push(0x46);
        out.push(disp as u8);
    }
}

/// `while (<cond>) <body>` lowers to a test-first shape with the
/// initial jmp landing on the cond, the body and cmp run inline,
/// and a backward jcc closing the loop. MSC aligns the loop-top
/// to an even byte offset — if the position right after the
/// 2-byte forward jmp would be odd, MSC inserts a single NOP pad
/// (fixture 4096); when prior bytes already leave us even, the
/// nop is dropped (fixture 4097's for-loop shows the same rule).
///
/// ```text
/// eb <body[+pad]>     jmp short to cond
/// [90]                nop pad iff next byte would be at odd offset
/// <body>              loop body
/// <cmp>               cond comparison
/// <jcc> <-back>       jne/je back to body start
/// ```
fn emit_while(
    cond: &Cond,
    body_stmt: &Stmt,
    locals: &[Option<i32>],
    has_frame: bool,
    out: &mut Vec<u8>,
) {
    emit_loop(cond, &[body_stmt], locals, has_frame, out);
}

/// `for (<init>; <cond>; <step>) <body>` — MSC's layout (fixture
/// 4097):
/// ```text
/// <init>              init expression-statement
/// eb <step+body[+pad]>  jmp short to cond
/// [90]                nop pad iff alignment requires
/// <step>              step expression (interleaved BEFORE body in loop)
/// <body>              loop body
/// <cmp>               cond comparison
/// <jcc> <-back>       jne/je back to step start
/// ```
/// The "step before body" arrangement makes the initial jmp skip
/// the step on the first iteration only; later iterations execute
/// step, then fall into body, then cond. Same alignment rule as
/// `while` for the post-jmp pad.
fn emit_for(
    init: &Stmt,
    cond: &Cond,
    step: &Stmt,
    body_stmt: &Stmt,
    locals: &[Option<i32>],
    has_frame: bool,
    out: &mut Vec<u8>,
) {
    emit_stmt(init, locals, has_frame, out);
    // The looped section is `step; body;` — treated as a single
    // "loop body" for the shared shape helper.
    emit_loop(cond, &[step, body_stmt], locals, has_frame, out);
}

/// Shared loop emitter — handles the alignment-pad, body
/// concatenation, cmp+jcc tail, and backward-jcc displacement.
/// Both while-loops (single-body) and for-loops (step+body) route
/// through here.
fn emit_loop(
    cond: &Cond,
    body_segments: &[&Stmt],
    locals: &[Option<i32>],
    has_frame: bool,
    out: &mut Vec<u8>,
) {
    let mut body_buf = Vec::new();
    for seg in body_segments {
        emit_stmt(seg, locals, has_frame, &mut body_buf);
    }
    let mut cmp_buf = Vec::new();
    emit_cond_cmp(cond, &mut cmp_buf);

    // Alignment: position right after the 2-byte `eb XX` should be
    // even. If it would be odd, insert a NOP pad and bump the
    // forward jmp displacement by 1.
    let pos_after_jmp = out.len() + 2;
    let needs_pad = pos_after_jmp % 2 != 0;
    let pad = if needs_pad { 1 } else { 0 };

    let take_when_true = matches!(
        cond,
        Cond::Truthy(_) | Cond::Cmp { op: RelOp::Ne, .. }
    );
    let jcc_opcode = if take_when_true { 0x75 } else { 0x74 };

    let body_len = body_buf.len();
    let cmp_len = cmp_buf.len();
    let forward_disp = i8::try_from(body_len + pad)
        .expect("body+pad short enough for jmp rel8");
    out.push(0xEB);
    out.push(forward_disp as u8);
    if needs_pad {
        out.push(0x90);
    }
    out.extend_from_slice(&body_buf);
    out.extend_from_slice(&cmp_buf);
    let back_disp = -(i8::try_from(cmp_len + 2 + body_len)
        .expect("loop body+cmp short enough for jcc rel8"));
    out.push(jcc_opcode);
    out.push(back_disp as u8);
}

/// `do <body> while (<cond>);` (fixture 4098). When the body's last
/// instruction already sets ZF for the cond, MSC drops the explicit
/// cmp and chains the jcc directly off the body's flags. Today we
/// detect this peephole specifically for the
/// `do <local> = <local> ± 1; while (<same-local>);` shape — the
/// only shape any fixture exercises.
///
/// ```text
/// <body>              body (sets ZF if peephole applies)
/// [<cmp>]             cmp only when peephole doesn't apply
/// <jcc> <-back>       jne/je back to body
/// ```
fn emit_do_while(
    body_stmt: &Stmt,
    cond: &Cond,
    locals: &[Option<i32>],
    has_frame: bool,
    out: &mut Vec<u8>,
) {
    let mut body_buf = Vec::new();
    emit_stmt(body_stmt, locals, has_frame, &mut body_buf);
    let body_len = body_buf.len();
    let elide_cmp = body_sets_flags_for_cond(body_stmt, cond);
    let mut cmp_buf = Vec::new();
    if !elide_cmp {
        emit_cond_cmp(cond, &mut cmp_buf);
    }
    let cmp_len = cmp_buf.len();
    let take_when_true = matches!(
        cond,
        Cond::Truthy(_) | Cond::Cmp { op: RelOp::Ne, .. }
    );
    let jcc_opcode = if take_when_true { 0x75 } else { 0x74 };
    out.extend_from_slice(&body_buf);
    out.extend_from_slice(&cmp_buf);
    let back_disp = -(i8::try_from(cmp_len + 2 + body_len)
        .expect("loop body+cmp short enough for jcc rel8"));
    out.push(jcc_opcode);
    out.push(back_disp as u8);
}

/// True when the body's last operation sets ZF appropriately for
/// the cond, so MSC can omit the explicit cmp in a `do-while` loop.
/// Current trigger: `<local> = <local> ± 1;` paired with
/// `while (<same-local>);`. Fixture 4098.
fn body_sets_flags_for_cond(body: &Stmt, cond: &Cond) -> bool {
    let Stmt::Assign { local_idx, value } = body else { return false };
    let Cond::Truthy(Expr::Local(cond_idx)) = cond else { return false };
    if local_idx != cond_idx {
        return false;
    }
    let Expr::BinOp { op, left, right } = value else { return false };
    if !matches!(op, BinOp::Add | BinOp::Sub) {
        return false;
    }
    matches!(left.as_ref(), Expr::Local(li) if li == local_idx)
        && matches!(right.as_ref(), Expr::IntLit(1))
}

/// Just the cmp half of a cond — used by `emit_while` which pairs
/// the comparison with a backward jcc rather than a forward skip.
fn emit_cond_cmp(cond: &Cond, out: &mut Vec<u8>) {
    match cond {
        Cond::Truthy(Expr::Local(idx)) => emit_cmp_local_imm(*idx, 0, out),
        Cond::Cmp { op: _, left: Expr::Local(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Local(idx) } => {
            emit_cmp_local_imm(*idx, *k, out);
        }
        other => panic!("Slice 5 cond cmp not yet supported: {other:?}"),
    }
}

/// Emit `cmp <X>, <Y>; <inverted-jcc> skip` where `skip` is a
/// forward `rel8` displacement equal to `take_then_disp`. The
/// caller has pre-computed the size of the then-body so we can use
/// the 2-byte jcc form without a fixup. Fixtures 4090 / 4091 / 4092.
fn emit_cond_skip(cond: &Cond, take_then_disp: i8, out: &mut Vec<u8>) {
    match cond {
        Cond::Truthy(Expr::Local(idx)) => {
            // `if (<local>)` → `cmp word ptr [bp-disp], 0; je skip`.
            emit_cmp_local_imm(*idx, 0, out);
            out.push(0x74); // je rel8
            out.push(take_then_disp as u8);
        }
        Cond::Cmp { op: RelOp::Eq, left: Expr::Local(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: RelOp::Eq, left: Expr::IntLit(k), right: Expr::Local(idx) } => {
            // `if (<local> == K)` → `cmp <local>, K; jne skip`.
            emit_cmp_local_imm(*idx, *k, out);
            out.push(0x75); // jne rel8
            out.push(take_then_disp as u8);
        }
        Cond::Cmp { op: RelOp::Ne, left: Expr::Local(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: RelOp::Ne, left: Expr::IntLit(k), right: Expr::Local(idx) } => {
            emit_cmp_local_imm(*idx, *k, out);
            out.push(0x74); // je rel8 — inverted from the != we want
            out.push(take_then_disp as u8);
        }
        other => panic!("Slice 5 cond shape not yet supported: {other:?}"),
    }
}

/// `cmp word ptr [bp-disp], imm` — MSC picks the `83 /7 r/m imm8sx`
/// form when the immediate fits in a sign-extended byte (which is
/// every fixture exercised by Slice 5 today). The 5-byte
/// `81 7e disp imm16` form is reserved for larger constants.
fn emit_cmp_local_imm(idx: usize, k: i32, out: &mut Vec<u8>) {
    let disp = -(i16::try_from(idx + 1).expect("local index fits") * 2);
    if let Ok(k_i8) = i8::try_from(k) {
        out.push(0x83);
        out.push(0x7E);          // ModR/M: mod=01 reg=111 (Grp1/7=CMP) r/m=110 (BP+disp8)
        out.push(disp as u8);
        out.push(k_i8 as u8);
    } else {
        let k16 = (k as u32 & 0xFFFF) as u16;
        out.push(0x81);
        out.push(0x7E);
        out.push(disp as u8);
        out.extend_from_slice(&k16.to_le_bytes());
    }
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
