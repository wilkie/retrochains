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
    let unit = parse_unit(&source)?;
    let bytes = build_obj(source_filename, &unit);
    let basename = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("OUT");
    let out_name = format!("{}.OBJ", basename.to_ascii_uppercase());
    std::fs::write(&out_name, bytes).map_err(EmitError::Io)?;
    Ok(std::path::PathBuf::from(out_name))
}

/// A translation unit: file-scope globals + function definitions
/// plus a shared pool of interned string literals.
#[derive(Debug, Clone)]
pub struct Unit {
    /// File-scope `int <name> [= <int>];` declarations in source
    /// order. Initialized globals contribute PUBDEFs + _DATA bytes;
    /// uninitialized globals (tentative definitions) come with a
    /// later fixture and use COMDEF instead.
    pub globals: Vec<Global>,
    pub functions: Vec<Function>,
    /// Each string is the bytes between the source double-quotes
    /// PLUS a terminating NUL byte appended by the parser.
    pub strings: Vec<Vec<u8>>,
}

/// A file-scope global variable. Phase 1 covers scalar `int g [= K];`
/// and array `int a[N];` (uninit only so far — fixture 4107).
#[derive(Debug, Clone)]
pub struct Global {
    pub name: String,
    /// `Some(vec)` for an explicit initializer. Each slot is one
    /// 2-byte word in `_DATA`; mixed lists carry int literals or
    /// CONST-segment string addresses. `None` is the tentative form
    /// (`int g;` / `int a[N];`) which lowers to a COMDEF (fixtures
    /// 4105, 4107).
    pub init: Option<Vec<GlobalInit>>,
    /// Array element count. `1` for scalar `int g;`. The COMDEF or
    /// _DATA byte-length is `2 * array_len`.
    pub array_len: usize,
}

#[derive(Debug, Clone)]
pub enum GlobalInit {
    /// Plain int literal — stored as 16-bit LE in `_DATA`.
    Int(i32),
    /// CONST-segment string address — stored as a 2-byte placeholder
    /// with a FIXUP that the linker resolves to DGROUP:CONST+offset.
    /// `usize` indexes into `Unit::strings`. Fixture 4110.
    StrAddr(usize),
}

/// One function definition. `return_int` distinguishes `int f(void)`
/// from `void f(void)` — void functions skip the return-value
/// instruction in their tail. `params` carries each parameter name
/// (all params are 16-bit int in Phase 1).
#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub return_int: bool,
    pub params: Vec<String>,
    pub locals: Vec<Option<i32>>,
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
    /// An expression statement — currently used only for
    /// discarded call results (`f();`, fixture 4099).
    ExprStmt(Expr),
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
        target: AssignTarget,
        value: Expr,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum AssignTarget {
    Local(usize),
    Global(usize),
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
/// land — Slice 3 had `IntLit` and `Local`; Slice 4 adds `BinOp`;
/// Slice 6 adds `Call`.
#[derive(Debug, Clone)]
pub enum Expr {
    /// A 16-bit-truncated int literal.
    IntLit(i32),
    /// Reference to a local by index into the enclosing function's
    /// `locals` array. Loaded from `[bp - 2*(idx+1)]`.
    Local(usize),
    /// Reference to a parameter by index into the enclosing
    /// function's `params` array. Loaded from `[bp + 4 + 2*idx]`
    /// (positive disp from BP since params live above the saved BP
    /// and the return address).
    Param(usize),
    /// A binary operation. `op` selects add/sub/mul/...; codegen
    /// picks the actual instruction (inc/dec/shl/shift-add/imul)
    /// based on the operands.
    BinOp { op: BinOp, left: Box<Expr>, right: Box<Expr> },
    /// Call by name with arguments. cdecl: caller pushes args
    /// right-to-left then cleans up the stack with `add sp, N`
    /// after the call. Fixtures 4099 (zero-arg) through 4102.
    Call { name: String, args: Vec<Expr> },
    /// Reference to an interned string literal — index into
    /// `Unit::strings`. Loaded as `mov ax, offset DGROUP:<CONST+off>`
    /// with a segment-relative FIXUP. Fixture 4103.
    StrLit(usize),
    /// Reference to a file-scope global — index into `Unit::globals`.
    /// Reads lower to `a1 imm16` (mov ax, moffs16) with a FIXUP
    /// describing the global's address; writes lower to
    /// `c7 06 addr imm16`. Fixtures 4104, 4106.
    Global(usize),
    /// Array element access — `a[<expr>]`. Constant index folds to
    /// an `a1 imm16` load whose immediate is `2 * index` (linker adds
    /// the array base via the FIXUP). Variable index defers to a
    /// later sub-slice. Fixture 4109.
    Index { array: usize, index: Box<Expr> },
    /// `*<ptr>` — pointer dereference. Phase 1 supports only the
    /// `*<char-ptr-global>` form (fixture 4111).
    Deref { ptr: Box<Expr> },
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
            // Parameters carry an unknown value at compile time.
            Expr::Param(_) => None,
            Expr::BinOp { op, left, right } => {
                let l = left.fold(locals)?;
                let r = right.fold(locals)?;
                Some(match op {
                    BinOp::Add => l.wrapping_add(r),
                    BinOp::Sub => l.wrapping_sub(r),
                    BinOp::Mul => l.wrapping_mul(r),
                })
            }
            Expr::Call { .. } => None,
            Expr::StrLit(_) => None,
            Expr::Global(_) => None,
            Expr::Index { .. } => None,
            Expr::Deref { .. } => None,
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
    LBrack,
    RBrack,
    Semi,
    Assign,
    EqEq,
    NotEq,
    Plus,
    Minus,
    Star,
    Comma,
    /// A C string literal — bytes between matching double-quotes,
    /// without the surrounding quotes and without a terminator.
    /// The trailing NUL is appended by codegen when interning.
    StrLit(Vec<u8>),
    /// A preprocessor directive line (`#include <...>` etc.) — we
    /// don't actually process headers; the directive is captured
    /// so the tokenizer can swallow it whole and so future fixtures
    /// that depend on specific declarations have a hook. Phase 1
    /// treats every `#include` as a no-op for the purposes of
    /// parsing, since `printf` and friends are recognized by name.
    PreprocLine,
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
            b'[' => { toks.push(Tok::LBrack); i += 1; }
            b']' => { toks.push(Tok::RBrack); i += 1; }
            b';' => { toks.push(Tok::Semi); i += 1; }
            b',' => { toks.push(Tok::Comma); i += 1; }
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
            b'#' => {
                // Treat the entire line as a preprocessor directive
                // (consume up to but not including the newline).
                // Phase 1 doesn't actually process any directive;
                // <stdio.h> definitions for printf are implicit.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                toks.push(Tok::PreprocLine);
            }
            b'"' => {
                // String literal — collect bytes until the closing
                // quote. Handles common C escapes (`\n`, `\t`, `\\`,
                // `\"`, `\0`).
                i += 1;
                let mut buf = Vec::new();
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        let esc = bytes[i + 1];
                        let translated = match esc {
                            b'n' => 0x0A,
                            b't' => 0x09,
                            b'r' => 0x0D,
                            b'0' => 0x00,
                            b'\\' => b'\\',
                            b'"' => b'"',
                            _ => {
                                return Err(EmitError::Unsupported(format!(
                                    "unknown escape `\\{}`",
                                    esc as char
                                )));
                            }
                        };
                        buf.push(translated);
                        i += 2;
                    } else {
                        buf.push(bytes[i]);
                        i += 1;
                    }
                }
                if i >= bytes.len() {
                    return Err(EmitError::Unsupported("unterminated string literal".to_owned()));
                }
                i += 1; // consume closing "
                toks.push(Tok::StrLit(buf));
            }
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
                    "char" => Tok::Kw("char"),
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
    param_names: Vec<String>,
    /// File-scope global names in source order; the index doubles
    /// as the `Expr::Global(idx)` value.
    global_names: Vec<String>,
    /// Same source order, used to materialize the `Unit::globals`.
    globals: Vec<Global>,
    /// Strings interned across the whole translation unit. New
    /// string literals append; duplicates currently get distinct
    /// entries (no dedup yet — no fixture exercises a repeated
    /// literal).
    strings: Vec<Vec<u8>>,
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

/// Parse Phase 1's source-shape envelope: a sequence of function
/// definitions, each `<ret-type> <name>(void) { <body> }`. `ret-type`
/// is `int` or `void`; bodies follow the existing per-statement
/// grammar.
fn parse_unit(source: &str) -> Result<Unit, EmitError> {
    let toks = tokenize(source)?;
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        local_names: Vec::new(),
        param_names: Vec::new(),
        global_names: Vec::new(),
        globals: Vec::new(),
        strings: Vec::new(),
    };
    let mut functions = Vec::new();
    while p.peek().is_some() {
        // Skip any preprocessor directives at file scope.
        if matches!(p.peek(), Some(Tok::PreprocLine)) {
            p.bump();
            continue;
        }
        // Disambiguate file-scope `int <name>...;` (global) from
        // `int <name>(...) { ... }` (function) by looking ahead.
        // `char *<name>` is unambiguously a global pointer.
        let is_int_global = matches!(p.peek(), Some(Tok::Kw("int")))
            && matches!(p.toks.get(p.pos + 1), Some(Tok::Ident(_)))
            && !matches!(p.toks.get(p.pos + 2), Some(Tok::LParen));
        let is_char_ptr_global = matches!(p.peek(), Some(Tok::Kw("char")))
            && matches!(p.toks.get(p.pos + 1), Some(Tok::Star));
        if is_int_global || is_char_ptr_global {
            parse_global_decl(&mut p)?;
            continue;
        }
        functions.push(parse_function(&mut p)?);
    }
    if functions.is_empty() {
        return Err(EmitError::Unsupported(
            "translation unit has no functions".to_owned(),
        ));
    }
    Ok(Unit { globals: p.globals, functions, strings: p.strings })
}

/// Parse one file-scope `<type> <name> [= <init>];` declaration and
/// register it in the parser's globals list. Phase 1 covers
/// `int <name>`, `int <name>[N]`, and `char *<name>` with optional
/// initializer. Caller has confirmed the next tokens form a
/// declaration, not a function.
fn parse_global_decl(p: &mut Parser<'_>) -> Result<(), EmitError> {
    // Type prefix. `int` is the only non-pointer type in Phase 1;
    // `char *` is the only pointer form so far.
    let is_pointer = match p.peek() {
        Some(Tok::Kw("int")) => {
            p.bump();
            false
        }
        Some(Tok::Kw("char")) => {
            p.bump();
            p.eat(&Tok::Star)?;
            true
        }
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected `int` or `char *` for global, got {other:?}"
            )));
        }
    };
    let name = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected global name, got {other:?}"
            )));
        }
    };
    // Optional `[N]` for an array declaration. The element count
    // determines the COMDEF or _DATA byte length.
    let array_len = if matches!(p.peek(), Some(Tok::LBrack)) {
        p.bump();
        let n = match p.bump().cloned() {
            Some(Tok::Int(k)) if k > 0 => k as usize,
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected positive array length, got {other:?}"
                )));
            }
        };
        p.eat(&Tok::RBrack)?;
        n
    } else {
        1
    };
    let init = if matches!(p.peek(), Some(Tok::Assign)) {
        p.bump();
        if matches!(p.peek(), Some(Tok::LBrace)) {
            p.bump();
            let mut values = Vec::new();
            loop {
                values.push(GlobalInit::Int(parse_signed_int(p)?));
                match p.peek() {
                    Some(Tok::Comma) => { p.bump(); }
                    Some(Tok::RBrace) => { p.bump(); break; }
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected `,` or `}}` in initializer, got {other:?}"
                        )));
                    }
                }
            }
            Some(values)
        } else if is_pointer && matches!(p.peek(), Some(Tok::StrLit(_))) {
            let bytes = match p.bump().cloned() {
                Some(Tok::StrLit(b)) => b,
                _ => unreachable!(),
            };
            let mut with_nul = bytes.clone();
            with_nul.push(0);
            let str_idx = p.strings.len();
            p.strings.push(with_nul);
            Some(vec![GlobalInit::StrAddr(str_idx)])
        } else {
            Some(vec![GlobalInit::Int(parse_signed_int(p)?)])
        }
    } else {
        None
    };
    p.eat(&Tok::Semi)?;
    p.global_names.push(name.clone());
    p.globals.push(Global { name, init, array_len });
    Ok(())
}

fn parse_function(p: &mut Parser<'_>) -> Result<Function, EmitError> {
    // `<ret-type> <name>(void) {`. Phase 1 supports `int` and `void`
    // as return types; no parameters yet.
    let return_int = match p.bump().cloned() {
        Some(Tok::Kw("int")) => true,
        Some(Tok::Kw("void")) => false,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected return type, got {other:?}"
            )));
        }
    };
    let name = match p.bump().cloned() {
        Some(Tok::Kw("main")) => "main".to_owned(),
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected function name, got {other:?}"
            )));
        }
    };
    p.eat(&Tok::LParen)?;
    // Parameter list: either `void` (no params) or one or more
    // `int <name>` separated by `,`. Phase 1 only handles int
    // parameters; other types come with later fixtures.
    let params = if matches!(p.peek(), Some(Tok::Kw("void"))) {
        p.bump();
        Vec::new()
    } else {
        let mut names = Vec::new();
        loop {
            p.eat(&Tok::Kw("int"))?;
            let pname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected parameter name, got {other:?}"
                    )));
                }
            };
            names.push(pname);
            if matches!(p.peek(), Some(Tok::Comma)) {
                p.bump();
                continue;
            }
            break;
        }
        names
    };
    p.eat(&Tok::RParen)?;
    p.eat(&Tok::LBrace)?;

    // Reset per-function name lists, then populate with this
    // function's params before parsing the body.
    p.local_names.clear();
    p.param_names = params.clone();

    // `int <name> [= <int>];` declarations.
    let mut local_inits: Vec<Option<i32>> = Vec::new();
    while matches!(p.peek(), Some(Tok::Kw("int"))) {
        p.bump(); // int
        let lname = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected identifier in declaration, got {other:?}"
                )));
            }
        };
        let init = if matches!(p.peek(), Some(Tok::Assign)) {
            p.bump();
            Some(parse_signed_int(p)?)
        } else {
            None
        };
        p.eat(&Tok::Semi)?;
        p.local_names.push(lname);
        local_inits.push(init);
    }

    // Body statements until the closing `}`.
    let mut body = Vec::new();
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        body.push(parse_stmt(p)?);
    }
    p.eat(&Tok::RBrace)?;

    Ok(Function { name, return_int, params, locals: local_inits, body })
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
            // Either `<local> = <expr>;` (assignment) or
            // `<name>();` (call as an expression statement).
            // Peek ahead one token to disambiguate.
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                _ => unreachable!(),
            };
            if matches!(p.peek(), Some(Tok::LParen)) {
                p.bump(); // (
                let args = parse_call_args(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::ExprStmt(Expr::Call { name, args }));
            }
            let target = if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                AssignTarget::Local(idx)
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                AssignTarget::Global(idx)
            } else {
                return Err(EmitError::Unsupported(format!(
                    "assignment to unknown identifier `{name}`"
                )));
            };
            p.eat(&Tok::Assign)?;
            let value = parse_expr(p)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Assign { target, value })
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
    let target = if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
        AssignTarget::Local(idx)
    } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
        AssignTarget::Global(idx)
    } else {
        return Err(EmitError::Unsupported(format!(
            "unknown identifier `{name}` in for-clause"
        )));
    };
    p.eat(&Tok::Assign)?;
    let value = parse_expr(p)?;
    Ok(Stmt::Assign { target, value })
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
        Some(Tok::StrLit(mut bytes)) => {
            // Intern the literal in the unit-level string pool with
            // the terminating NUL appended. Fixture 4103.
            bytes.push(0);
            let idx = p.strings.len();
            p.strings.push(bytes);
            Ok(Expr::StrLit(idx))
        }
        Some(Tok::Minus) => match p.bump().cloned() {
            Some(Tok::Int(n)) => Ok(Expr::IntLit(-n)),
            other => Err(EmitError::Unsupported(format!(
                "expected int after unary -, got {other:?}"
            ))),
        },
        Some(Tok::Star) => {
            // Unary deref `*<expr>`.
            let inner = parse_atom(p)?;
            Ok(Expr::Deref { ptr: Box::new(inner) })
        }
        Some(Tok::Ident(name)) => {
            // Identifier may be a call site (`f(args)`), a local
            // reference, or a parameter reference. Disambiguate by
            // looking ahead for `(`.
            if matches!(p.peek(), Some(Tok::LParen)) {
                p.bump(); // (
                let args = parse_call_args(p)?;
                return Ok(Expr::Call { name, args });
            }
            if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                Ok(Expr::Local(idx))
            } else if let Some(idx) = p.param_names.iter().position(|n| *n == name) {
                Ok(Expr::Param(idx))
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                // `<global>[<expr>]` is an array index. The result is
                // an lvalue of the element type (always int in
                // Phase 1).
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    Ok(Expr::Index { array: idx, index: Box::new(index) })
                } else {
                    Ok(Expr::Global(idx))
                }
            } else {
                Err(EmitError::Unsupported(format!("unknown identifier `{name}`")))
            }
        }
        other => Err(EmitError::Unsupported(format!(
            "expected atom, got {other:?}"
        ))),
    }
}

/// Parse the contents of `(<expr>, <expr>, ...)` for a call site —
/// caller has already consumed the opening `(`. Stops at and
/// consumes the closing `)`.
fn parse_call_args(p: &mut Parser<'_>) -> Result<Vec<Expr>, EmitError> {
    let mut args = Vec::new();
    if matches!(p.peek(), Some(Tok::RParen)) {
        p.bump();
        return Ok(args);
    }
    loop {
        args.push(parse_expr(p)?);
        match p.peek() {
            Some(Tok::Comma) => {
                p.bump();
            }
            Some(Tok::RParen) => {
                p.bump();
                return Ok(args);
            }
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected `,` or `)` in call args, got {other:?}"
                )));
            }
        }
    }
}

/// Per-function emission output — the function's code bytes plus a
/// list of fixup-needing references (TU-local calls, external
/// calls, and string-pool loads). After the calling code knows
/// each function's global offset and each string's CONST offset,
/// fixups get either patched in-band (TU-local calls) or emitted
/// into the OBJ's FIXUPP record (external calls + string loads).
struct FunctionEmit {
    bytes: Vec<u8>,
    fixups: Vec<Fixup>,
}

#[derive(Debug)]
struct Fixup {
    /// Offset of the placeholder bytes within `FunctionEmit::bytes`.
    /// For `e8 disp16` calls this is the offset of the `e8` opcode
    /// (disp bytes at +1, +2); for `b8 imm16` string loads this is
    /// the offset of the `b8` opcode (imm bytes at +1, +2).
    body_offset: usize,
    kind: FixupKind,
}

#[derive(Debug)]
enum FixupKind {
    /// TU-local call: target's offset is known once all functions
    /// are emitted; the placeholder gets resolved in-band (no OMF
    /// FIXUP record).
    TuLocalCall { target: String },
    /// External call: target gets an EXTDEF entry and a self-rel
    /// FIXUP record (`84 off 56 idx`). The EXTDEF index is filled
    /// in after the table is finalized.
    ExtCall { target: String },
    /// Load of a string pool offset: `b8 imm16` patched at link time
    /// to the CONST offset, with a segment-relative FIXUP using
    /// pre-emitted threads (`c4 off 9c`).
    StrLoad { string_idx: usize },
    /// Reference to an initialized file-scope global at a known
    /// offset within `_DATA`. The FIXUP uses DGROUP-as-frame and
    /// _DATA-as-target via the pre-emitted threads (`c4 off 9d`).
    /// Fixtures 4104, 4106.
    GlobalAddr { global_idx: usize },
}

/// Same as `Fixup` but with the body_offset translated to the
/// LEDATA-relative offset (function_offset + body_offset).
#[derive(Debug)]
struct ResolvedFixup {
    ledata_offset: usize,
    kind: FixupKind,
}

/// Frame shape, which drives both the prologue and the
/// per-return epilogue. Picked once per function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// No prologue, no epilogue. Used for functions with neither
    /// locals nor parameters — e.g. fixtures 4075-4078 and 4099's
    /// `main`. Tail is just `c3` (ret).
    None,
    /// `push bp; mov bp, sp` prologue and `pop bp; ret` tail. Used
    /// for parameterized functions with no locals (fixtures 4100-
    /// 4102's callees). SP doesn't slide so no `mov sp, bp`.
    BpOnly,
    /// Full prologue (`push bp; mov bp, sp`) plus the locals-frame
    /// allocation via chkstk, and `mov sp, bp; pop bp; ret` tail.
    /// Used whenever the function has locals (fixtures 4079+).
    WithSlide,
}

impl Frame {
    fn for_function(func: &Function) -> Self {
        let has_locals = !func.locals.is_empty();
        let has_params = !func.params.is_empty();
        match (has_locals, has_params) {
            (true, _) => Frame::WithSlide,
            (false, true) => Frame::BpOnly,
            (false, false) => Frame::None,
        }
    }
    fn epilogue_bytes(self) -> &'static [u8] {
        match self {
            Frame::None => &[0xC3],
            Frame::BpOnly => &[0x5D, 0xC3],
            Frame::WithSlide => &[0x8B, 0xE5, 0x5D, 0xC3],
        }
    }
}

/// Produce the OBJ bytes for `cl /c /AS <source>` compiling the
/// translation unit `unit`. `source_filename` goes into THEADR
/// uppercased the same way CL does it on the command line.
#[must_use]
pub fn build_obj(source_filename: &str, unit: &Unit) -> Vec<u8> {
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

    // Build each function's body up front so we can stamp the
    // total length into the _TEXT SEGDEF and compute per-function
    // offsets for call resolution + chkstk FIXUPs.
    let function_emits: Vec<FunctionEmit> = unit
        .functions
        .iter()
        .map(emit_function)
        .collect();

    // Per-function global offset within the _TEXT segment.
    let mut function_offsets: Vec<usize> = Vec::with_capacity(unit.functions.len());
    let mut cursor: usize = 0;
    let mut offset_by_name: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut defined_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, fe) in function_emits.iter().enumerate() {
        function_offsets.push(cursor);
        let sym = symbol_name(&unit.functions[i].name);
        offset_by_name.insert(sym.clone(), cursor);
        defined_names.insert(sym);
        cursor += fe.bytes.len();
    }
    let total_code_bytes = cursor;
    let text_len = u16::try_from(total_code_bytes).expect("_TEXT body fits in u16");

    // String-pool offsets in CONST. Strings live back-to-back in
    // source order; the segment's total byte length stamps into
    // the CONST SEGDEF.
    let mut string_offsets: Vec<usize> = Vec::with_capacity(unit.strings.len());
    let mut const_cursor: usize = 0;
    for s in &unit.strings {
        string_offsets.push(const_cursor);
        const_cursor += s.len();
    }
    let const_len = u16::try_from(const_cursor).expect("CONST length fits in u16");
    let _ = string_offsets; // not used directly yet; future fixtures with > 1 string will pick this up

    // _DATA layout — every initialized global gets 2 bytes (int) in
    // source order. Uninitialized globals (tentative definitions)
    // don't contribute here; they'll go through COMDEF in a later
    // sub-slice.
    let mut data_offsets: Vec<Option<usize>> = Vec::with_capacity(unit.globals.len());
    let mut data_cursor: usize = 0;
    for g in &unit.globals {
        if g.init.is_some() {
            data_offsets.push(Some(data_cursor));
            data_cursor += g.array_len * 2;
        } else {
            data_offsets.push(None);
        }
    }
    let data_len = u16::try_from(data_cursor).expect("_DATA fits in u16");

    // Discover true externs: any TuLocalCall fixup whose target is
    // not defined in this unit. (chkstk is recorded as ExtCall and
    // routes through the system-extern slot below.) Preserve
    // first-reference order so MSC's EXTDEF layout matches.
    let mut user_extern_order: Vec<String> = Vec::new();
    let mut seen_externs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for fe in &function_emits {
        for fx in &fe.fixups {
            if let FixupKind::TuLocalCall { target } = &fx.kind
                && !defined_names.contains(target)
                && seen_externs.insert(target.clone())
            {
                user_extern_order.push(target.clone());
            }
        }
    }

    // SEGDEF table. MSC uses acbp=0x48 for every segment in the
    // small model.
    //
    // SEGDEF #1: _TEXT  — code, total padded function bytes
    b.write_segdef16(0x48, text_len, 3, 4, 1);
    // SEGDEF #2: _DATA  — initialized globals, 2 bytes each in
    // source order
    b.write_segdef16(0x48, data_len, 5, 6, 1);
    // SEGDEF #3: CONST  — read-only literals; length = string-pool
    // total (fixture 4103: `"hi\0"` = 3 bytes)
    b.write_segdef16(0x48, const_len, 7, 7, 1);
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

    // Tentative-def globals → COMDEF. Track their indices into
    // unit.globals; we'll emit a COMDEF record between two EXTDEF
    // records and slot the symbols into the same EXTDEF-index space.
    let comdef_globals: Vec<usize> = unit
        .globals
        .iter()
        .enumerate()
        .filter_map(|(i, g)| if g.init.is_none() { Some(i) } else { None })
        .collect();

    // EXTDEF + (optional) COMDEF layout, picked based on what
    // symbols this TU references:
    //
    //   No user externs, no COMDEFs (fixture 4099): single EXTDEF
    //     with __acrtused, __chkstk, then function-name EXTDEFs.
    //
    //   No user externs, has COMDEFs (fixture 4105): EXTDEF1 with
    //     __acrtused + __chkstk, then COMDEF for the tentative
    //     globals, then EXTDEF2 with function names.
    //
    //   Has user externs (fixture 4103): __acrtused, user externs,
    //     function names, __chkstk — all in one EXTDEF.
    let mut extdef_idx_of: std::collections::HashMap<String, u8> =
        std::collections::HashMap::new();
    let mut next_idx: u8 = 1;
    let emit_group = |b: &mut ObjBuilder,
                          entries: &[(String, u8)],
                          idx_map: &mut std::collections::HashMap<String, u8>,
                          start: &mut u8| {
        if entries.is_empty() {
            return;
        }
        let mut payload = Vec::new();
        for (name, ty) in entries {
            payload.push(u8::try_from(name.len()).expect("EXTDEF name fits"));
            payload.extend_from_slice(name.as_bytes());
            payload.push(*ty);
            idx_map.insert(name.clone(), *start);
            *start += 1;
        }
        b.write_record(obj::EXTDEF, &payload);
    };
    if user_extern_order.is_empty() {
        if comdef_globals.is_empty() {
            // No splits — single combined EXTDEF.
            let mut entries: Vec<(String, u8)> = Vec::new();
            entries.push(("__acrtused".to_owned(), 0x01));
            entries.push(("__chkstk".to_owned(), 0x00));
            for f in &unit.functions {
                entries.push((symbol_name(&f.name), 0x00));
            }
            emit_group(&mut b, &entries, &mut extdef_idx_of, &mut next_idx);
        } else {
            let pre =
                vec![("__acrtused".to_owned(), 0x01), ("__chkstk".to_owned(), 0x00)];
            emit_group(&mut b, &pre, &mut extdef_idx_of, &mut next_idx);
            let mut payload = Vec::new();
            for &gi in &comdef_globals {
                let g = &unit.globals[gi];
                let sym = symbol_name(&g.name);
                let byte_len = g.array_len * 2;
                payload.push(u8::try_from(sym.len()).expect("COMDEF name fits"));
                payload.extend_from_slice(sym.as_bytes());
                payload.push(0x00); // type index
                payload.push(0x62); // NEAR data
                // Length encoded: single byte for ≤0x80, otherwise
                // 0x81 + LE u16. Fixture 4107 sits in the small bucket.
                if byte_len <= 0x80 {
                    payload.push(byte_len as u8);
                } else {
                    payload.push(0x81);
                    payload.extend_from_slice(&u16::try_from(byte_len)
                        .expect("COMDEF u16 length fits")
                        .to_le_bytes());
                }
                extdef_idx_of.insert(sym, next_idx);
                next_idx += 1;
            }
            b.write_record(0xB0, &payload);
            let post: Vec<(String, u8)> = unit
                .functions
                .iter()
                .map(|f| (symbol_name(&f.name), 0x00))
                .collect();
            emit_group(&mut b, &post, &mut extdef_idx_of, &mut next_idx);
        }
    } else {
        let mut entries: Vec<(String, u8)> = Vec::new();
        entries.push(("__acrtused".to_owned(), 0x01));
        for name in &user_extern_order {
            entries.push((name.clone(), 0x00));
        }
        for f in &unit.functions {
            entries.push((symbol_name(&f.name), 0x00));
        }
        entries.push(("__chkstk".to_owned(), 0x00));
        emit_group(&mut b, &entries, &mut extdef_idx_of, &mut next_idx);
    }

    // PUBDEFs — one record per (base-group, base-seg) bucket.
    // Globals live at DGROUP:_DATA = (group 1, seg 2). Functions
    // live at 0:_TEXT = (group 0, seg 1). MSC emits the globals
    // PUBDEF first when both exist (source order matches: globals
    // are declared at file scope before functions). Fixtures 4104,
    // 4106.
    let has_init_globals = data_cursor > 0;
    if has_init_globals {
        let mut payload = Vec::new();
        payload.push(1); // base group idx (DGROUP)
        payload.push(2); // base segment idx (_DATA)
        for (i, g) in unit.globals.iter().enumerate() {
            if let Some(off) = data_offsets[i] {
                let sym = symbol_name(&g.name);
                let off = u16::try_from(off).expect("offset fits");
                payload.push(u8::try_from(sym.len()).expect("pubdef name fits"));
                payload.extend_from_slice(sym.as_bytes());
                payload.extend_from_slice(&off.to_le_bytes());
                payload.push(0); // type idx
            }
        }
        b.write_record(obj::PUBDEF_16, &payload);
    }
    {
        let mut payload = Vec::new();
        payload.push(0); // base group idx
        payload.push(1); // base segment idx (_TEXT)
        for (i, f) in unit.functions.iter().enumerate() {
            let sym = symbol_name(&f.name);
            let off = u16::try_from(function_offsets[i]).expect("offset fits");
            payload.push(u8::try_from(sym.len()).expect("pubdef name fits"));
            payload.extend_from_slice(sym.as_bytes());
            payload.extend_from_slice(&off.to_le_bytes());
            payload.push(0); // type idx
        }
        b.write_record(obj::PUBDEF_16, &payload);
    }

    // COMENT class 0xA2 — link-pass marker. MSC sandwiches the
    // LEDATA records between EXTDEF/PUBDEF setup and the data
    // itself. The payload byte 0x01 is the "start of data" marker;
    // the matching 0xA2 with 0x00 doesn't appear in this OBJ
    // because there's only one LEDATA pass.
    b.write_coment(&[0x00, 0xA2, 0x01]);

    // Walk every function's fixups: TuLocalCall fixups whose target
    // IS defined in this unit get patched in-band (intra-segment
    // self-rel displacement). The remainder (ExtCall + StrLoad) are
    // collected with their LEDATA-relative offsets for the FIXUPP
    // record.
    let mut function_emits = function_emits;
    let mut ledata_fixups: Vec<ResolvedFixup> = Vec::new();
    for (i, fe) in function_emits.iter_mut().enumerate() {
        let caller_off = function_offsets[i];
        for fx in &fe.fixups {
            match &fx.kind {
                FixupKind::TuLocalCall { target } if defined_names.contains(target) => {
                    let target_off = offset_by_name
                        .get(target)
                        .copied()
                        .expect("defined names map covers this target");
                    let disp = (target_off as i32)
                        - (caller_off as i32 + fx.body_offset as i32 + 3);
                    let disp16 = (disp as i32 & 0xFFFF) as u16;
                    fe.bytes[fx.body_offset + 1] = (disp16 & 0xFF) as u8;
                    fe.bytes[fx.body_offset + 2] = ((disp16 >> 8) & 0xFF) as u8;
                }
                FixupKind::TuLocalCall { target } => {
                    // True external call: route through the OMF
                    // FIXUPP machinery. Reclassify as ExtCall so
                    // the offset-emission loop handles it uniformly.
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset + 1,
                        kind: FixupKind::ExtCall { target: target.clone() },
                    });
                }
                FixupKind::ExtCall { target } => {
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset + 1,
                        kind: FixupKind::ExtCall { target: target.clone() },
                    });
                }
                FixupKind::StrLoad { string_idx } => {
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset + 1,
                        kind: FixupKind::StrLoad { string_idx: *string_idx },
                    });
                }
                FixupKind::GlobalAddr { global_idx } => {
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset + 1,
                        kind: FixupKind::GlobalAddr { global_idx: *global_idx },
                    });
                }
            }
        }
    }

    // LEDATA — CONST segment first (its bytes are referenced by any
    // FIXUPs inside _DATA, so MSC orders it ahead).
    if const_len > 0 {
        let mut const_bytes: Vec<u8> = Vec::with_capacity(const_cursor);
        for s in &unit.strings {
            const_bytes.extend_from_slice(s);
        }
        b.write_ledata16(3, 0, &const_bytes);
    }

    // LEDATA — _DATA segment, initialized global values. MSC packs
    // them sequentially in source order, little-endian. StrAddr
    // slots emit a 2-byte placeholder and pick up a FIXUPP record
    // emitted immediately after the LEDATA.
    if data_cursor > 0 {
        let mut data_bytes: Vec<u8> = Vec::with_capacity(data_cursor);
        for g in &unit.globals {
            if let Some(values) = &g.init {
                for v in values {
                    match v {
                        GlobalInit::Int(k) => {
                            let v16 = (*k as u32 & 0xFFFF) as u16;
                            data_bytes.extend_from_slice(&v16.to_le_bytes());
                        }
                        GlobalInit::StrAddr(_) => {
                            data_bytes.extend_from_slice(&[0, 0]);
                        }
                    }
                }
            }
        }
        b.write_ledata16(2, 0, &data_bytes);
        let mut data_fixups: Vec<u8> = Vec::new();
        let mut off: usize = 0;
        for g in &unit.globals {
            if let Some(values) = &g.init {
                for v in values {
                    if let GlobalInit::StrAddr(_) = v {
                        // `c4 off 9c` — frame thread 1 (DGROUP),
                        // target thread 0 (CONST), no displacement.
                        data_fixups.extend_from_slice(&[0xC4,
                            u8::try_from(off).expect("data fixup offset fits"),
                            0x9C]);
                    }
                    off += 2;
                }
            }
        }
        if !data_fixups.is_empty() {
            b.write_fixupp(&data_fixups);
        }
    }

    // LEDATA — _TEXT segment, the concatenated function bodies.
    let mut all_code = Vec::with_capacity(total_code_bytes);
    for fe in &function_emits {
        all_code.extend_from_slice(&fe.bytes);
    }
    b.write_ledata16(1, 0, &all_code);

    // FIXUPP — every ExtCall + StrLoad fixup needs a subrecord.
    // MSC sorts by descending LEDATA offset (fixture 4103's order
    // is offset 10, 6, 3). Each FIXUP subrecord's shape:
    //   ExtCall: `84 off 56 <extdef_idx>` (self-rel to EXTDEF)
    //   StrLoad: `c4 off 9c` (seg-rel via DGROUP/CONST threads)
    ledata_fixups.sort_by(|a, b| b.ledata_offset.cmp(&a.ledata_offset));
    let mut fixup_payload = Vec::new();
    for rf in &ledata_fixups {
        let off = u8::try_from(rf.ledata_offset).expect("fixup offset fits in u8");
        match &rf.kind {
            FixupKind::ExtCall { target } => {
                let idx = *extdef_idx_of
                    .get(target)
                    .unwrap_or_else(|| panic!("EXTDEF index missing for `{target}`"));
                fixup_payload.extend_from_slice(&[0x84, off, 0x56, idx]);
            }
            FixupKind::StrLoad { .. } => {
                fixup_payload.extend_from_slice(&[0xC4, off, 0x9C]);
            }
            FixupKind::GlobalAddr { global_idx } => {
                if unit.globals[*global_idx].init.is_some() {
                    // Init global → PUBDEF in DGROUP:_DATA. Frame
                    // thread 1 (DGROUP) + target thread 1 (_DATA),
                    // no displacement; the linker substitutes the
                    // global's _DATA-relative offset.
                    fixup_payload.extend_from_slice(&[0xC4, off, 0x9D]);
                } else {
                    // Tentative def → COMDEF. Explicit frame method
                    // 5 (target's frame), explicit target via EXTDEF
                    // index, no displacement.
                    let sym = symbol_name(&unit.globals[*global_idx].name);
                    let idx = *extdef_idx_of
                        .get(&sym)
                        .unwrap_or_else(|| panic!("EXTDEF index missing for COMDEF `{sym}`"));
                    fixup_payload.extend_from_slice(&[0xC4, off, 0x56, idx]);
                }
            }
            FixupKind::TuLocalCall { .. } => unreachable!(),
        }
    }
    b.write_fixupp(&fixup_payload);

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
/// Forward-substitute reads of file-scope globals with the
/// constant most recently assigned to them. MSC performs this fold
/// across straight-line statements within a function — fixture 4106
/// (`g = 5; return g;` becomes `mov ax, 5` instead of `mov ax, [g]`).
/// Control flow drops the known-value table conservatively (a real
/// pass would re-merge across branches; the only fixture so far is
/// straight-line so we keep the implementation small).
fn const_prop_globals(stmts: &[Stmt]) -> Vec<Stmt> {
    use std::collections::HashMap;
    let mut known: HashMap<usize, i32> = HashMap::new();
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        let mut new_stmt = stmt.clone();
        match &mut new_stmt {
            Stmt::Return(e) => fold_globals_expr(e, &known),
            Stmt::ExprStmt(e) => fold_globals_expr(e, &known),
            Stmt::Assign { target, value } => {
                fold_globals_expr(value, &known);
                if let AssignTarget::Global(g) = target {
                    if let Expr::IntLit(k) = value {
                        known.insert(*g, *k);
                    } else {
                        known.remove(g);
                    }
                }
            }
            Stmt::Empty => {}
            _ => {
                // Conservative: anything with branches/loops invalidates
                // every global's known value.
                known.clear();
            }
        }
        out.push(new_stmt);
    }
    out
}

fn fold_globals_expr(e: &mut Expr, known: &std::collections::HashMap<usize, i32>) {
    match e {
        Expr::Global(idx) => {
            if let Some(&k) = known.get(idx) {
                *e = Expr::IntLit(k);
            }
        }
        Expr::BinOp { left, right, .. } => {
            fold_globals_expr(left, known);
            fold_globals_expr(right, known);
        }
        Expr::Call { args, .. } => {
            for a in args {
                fold_globals_expr(a, known);
            }
        }
        Expr::Index { index, .. } => {
            fold_globals_expr(index, known);
        }
        Expr::Deref { ptr } => {
            fold_globals_expr(ptr, known);
        }
        Expr::IntLit(_) | Expr::Local(_) | Expr::Param(_) | Expr::StrLit(_) => {}
    }
}

fn emit_function(func: &Function) -> FunctionEmit {
    let body = const_prop_globals(&func.body);
    let mut bytes = Vec::with_capacity(32);
    let mut fixups: Vec<Fixup> = Vec::new();
    let frame = Frame::for_function(func);
    let frame_bytes = func.locals.len() * 2;

    match frame {
        Frame::None => bytes.extend_from_slice(&[0x33, 0xC0]),
        Frame::BpOnly => bytes.extend_from_slice(&[0x55, 0x8B, 0xEC, 0x33, 0xC0]),
        Frame::WithSlide => {
            bytes.extend_from_slice(&[0x55, 0x8B, 0xEC]);
            bytes.push(0xB8);
            bytes.extend_from_slice(
                &u16::try_from(frame_bytes)
                    .expect("frame fits in u16")
                    .to_le_bytes(),
            );
        }
    }
    // Call to __chkstk — emit a placeholder `e8 00 00` and record
    // a Fixup so the post-pass knows to write the EXTDEF-relative
    // FIXUPP record (chkstk is always external; resolved via the
    // OMF FIXUPP machinery, not in-band).
    let body_offset = bytes.len();
    bytes.extend_from_slice(&[0xE8, 0x00, 0x00]);
    fixups.push(Fixup {
        body_offset,
        kind: FixupKind::ExtCall { target: "__chkstk".to_owned() },
    });

    // Initialized-local writes — `int x = K;` → `c7 46 disp lo hi`.
    for (i, init) in func.locals.iter().enumerate() {
        if let Some(value) = init {
            let disp = -(i16::try_from(i + 1).expect("local index fits") * 2);
            let imm = (*value as u32 & 0xFFFF) as u16;
            bytes.push(0xC7);
            bytes.push(0x46);
            bytes.push(disp as u8);
            bytes.extend_from_slice(&imm.to_le_bytes());
        }
    }

    let mut reachable = true;
    for stmt in &body {
        if !reachable {
            break;
        }
        emit_stmt(
            stmt,
            &func.locals,
            frame,
            func.return_int,
            &mut bytes,
            &mut fixups,
        );
        if stmt_always_returns(stmt, &func.locals) {
            reachable = false;
        }
    }

    // Implicit return at the end of void functions that don't have
    // an explicit `return;`. MSC's `_f` body in fixture 4099 ends
    // with `c3` after the chkstk call. The epilogue shape follows
    // the function's frame.
    if reachable && !func.return_int {
        bytes.extend_from_slice(frame.epilogue_bytes());
    }

    if bytes.len() % 2 != 0 {
        bytes.push(0x90);
    }

    FunctionEmit { bytes, fixups }
}

/// Mangle a C function name into the OBJ symbol it produces.
/// MSC's small-model convention prefixes every function with `_`.
fn symbol_name(c_name: &str) -> String {
    format!("_{c_name}")
}

/// Emit a single statement (recursive: if-statements contain
/// nested statements). Returns no value — appends directly to `out`.
fn emit_stmt(
    stmt: &Stmt,
    locals: &[Option<i32>],
    frame: Frame,
    return_int: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    match stmt {
        Stmt::Return(expr) => emit_return(expr, locals, frame, return_int, out, fixups),
        Stmt::Empty => {}
        Stmt::ExprStmt(Expr::Call { name, args }) => {
            emit_call(name, args, locals, out, fixups);
        }
        Stmt::ExprStmt(other) => {
            panic!("ExprStmt with non-call expression not yet supported: {other:?}");
        }
        Stmt::Assign { target, value } => emit_assign(*target, value, locals, out, fixups),
        Stmt::While { cond, body } => {
            emit_while(cond, body, locals, frame, return_int, out, fixups);
        }
        Stmt::DoWhile { body, cond } => {
            emit_do_while(body, cond, locals, frame, return_int, out, fixups);
        }
        Stmt::For { init, cond, step, body } => {
            emit_for(init, cond, step, body, locals, frame, return_int, out, fixups);
        }
        Stmt::If { cond, then_branch, else_branch } => {
            // Constant-condition elision: when the cond folds to a
            // compile-time integer, MSC keeps only the live branch
            // and drops the comparison + jump entirely. Fixtures
            // 4094 (if (0)) and 4095 (if (1)) confirm.
            if let Some(k) = fold_cond(cond, locals) {
                if k != 0 {
                    emit_stmt(then_branch, locals, frame, return_int, out, fixups);
                } else if let Some(else_branch) = else_branch {
                    emit_stmt(else_branch, locals, frame, return_int, out, fixups);
                }
                return;
            }
            // Build the then-branch into a scratch buffer so we know
            // its byte count for the conditional-jump displacement.
            let mut then_buf = Vec::new();
            let mut then_fixups = Vec::new();
            emit_stmt(then_branch, locals, frame, return_int, &mut then_buf, &mut then_fixups);
            let then_len = then_buf.len();
            let take_then_disp = i8::try_from(then_len)
                .expect("then-body short enough for jcc rel8");
            emit_cond_skip(cond, take_then_disp, out);
            // Bring any then-branch call sites into the parent buffer,
            // offsetting their body_offset by where the then bytes
            // land in `out`.
            let then_base = out.len();
            out.extend_from_slice(&then_buf);
            for mut c in then_fixups {
                c.body_offset += then_base;
                fixups.push(c);
            }
            if let Some(else_branch) = else_branch {
                emit_stmt(else_branch, locals, frame, return_int, out, fixups);
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
        Stmt::ExprStmt(_) | Stmt::Assign { .. } => false,
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

fn emit_return(
    expr: &Expr,
    locals: &[Option<i32>],
    frame: Frame,
    return_int: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    if return_int {
        // Return-of-call peephole: `return f(args);` leaves the
        // result in AX from the call's return value — no extra
        // load before ret. Fixture 4102 confirms.
        if let Expr::Call { name, args } = expr {
            emit_call(name, args, locals, out, fixups);
        } else if let Some(k) = expr.fold(locals) {
            if k == 0 {
                out.extend_from_slice(&[0x2B, 0xC0]);
            } else {
                let imm = (k as u32 & 0xFFFF) as u16;
                out.push(0xB8);
                out.extend_from_slice(&imm.to_le_bytes());
            }
        } else {
            emit_expr_to_ax(expr, locals, out, fixups);
        }
    }
    out.extend_from_slice(frame.epilogue_bytes());
}

/// `<name>(args)` — cdecl call. Args are evaluated in source order
/// but PUSHed right-to-left, then the call lands, then the caller
/// cleans up with `add sp, N`. Fixtures 4100, 4101, 4102.
///
/// 8086 has no `push imm16` opcode (added in 286+), so a constant
/// arg becomes `mov ax, K; push ax` (4 bytes). Local/param args go
/// through `push word ptr [bp+disp]` (3 bytes).
fn emit_call(
    name: &str,
    args: &[Expr],
    locals: &[Option<i32>],
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    for arg in args.iter().rev() {
        emit_push_arg(arg, locals, out, fixups);
    }
    let body_offset = out.len();
    out.extend_from_slice(&[0xE8, 0x00, 0x00]);
    // Both TU-local and external calls record their target name.
    // The classification (intra-segment patch vs OMF FIXUPP record)
    // happens in build_obj once the defined-function set is known.
    fixups.push(Fixup {
        body_offset,
        kind: FixupKind::TuLocalCall { target: symbol_name(name) },
    });
    let cleanup_bytes = args.len() * 2;
    if cleanup_bytes > 0 {
        // `add sp, imm8sx` — Grp1 r/m16,imm8sx with /0=ADD,
        // ModR/M mod=11 r/m=100 (SP). 3 bytes for small N.
        out.push(0x83);
        out.push(0xC4);
        out.push(u8::try_from(cleanup_bytes).expect("cleanup fits in u8"));
    }
}

/// Push one call argument onto the stack. For Phase 1: constants
/// via `mov ax, K; push ax`; locals/params via direct memory push;
/// string literals via `mov ax, <pool offset>; push ax` with a
/// FIXUP for the linker to fill in the actual offset.
fn emit_push_arg(arg: &Expr, _locals: &[Option<i32>], out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match arg {
        Expr::IntLit(k) => {
            let imm = (*k as u32 & 0xFFFF) as u16;
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
            out.push(0x50); // push ax
        }
        Expr::Local(idx) => {
            // `push word ptr [bp - 2*(idx+1)]` — `FF /6 r/m`.
            let disp = -(i16::try_from(idx + 1).expect("local index fits") * 2);
            out.push(0xFF);
            out.push(0x76);
            out.push(disp as u8);
        }
        Expr::Param(idx) => {
            let disp = i8::try_from(4 + (idx * 2)).expect("param disp fits");
            out.push(0xFF);
            out.push(0x76);
            out.push(disp as u8);
        }
        Expr::StrLit(string_idx) => {
            // `mov ax, 00 00` placeholder; FIXUPP makes the linker
            // write the CONST-segment offset (relative to DGROUP).
            // Fixture 4103.
            let body_offset = out.len();
            out.extend_from_slice(&[0xB8, 0x00, 0x00]);
            out.push(0x50); // push ax
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::StrLoad { string_idx: *string_idx },
            });
        }
        other => panic!("argument shape not yet supported: {other:?}"),
    }
}

/// `<local> = <expr>;`. Phase 1 supports the peephole
/// `<local> = <same-local> ± 1;` → `inc/dec word ptr [bp-disp]`
/// (fixture 4096: `x = x - 1;` in a while body). The general path
/// — `mov ax, <expr>; mov [bp-disp], ax` — is reserved for a
/// future fixture that exercises a non-peephole shape.
fn emit_assign(target: AssignTarget, value: &Expr, locals: &[Option<i32>], out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let local_idx = match target {
        AssignTarget::Local(i) => i,
        AssignTarget::Global(g) => {
            return emit_assign_global(g, value, locals, out, fixups);
        }
    };
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
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0x89);                       // MOV r/m16, r16  (AX → mem)
        out.push(0x46);
        out.push(disp as u8);
    }
}

/// `<global> = <expr>;`. Constant RHS → `c7 06 addr imm16`
/// (mov word ptr [imm16], imm16, 6 bytes); general RHS →
/// `<expr-to-ax>; a3 addr` (mov moffs16, ax, 3 bytes).
/// Both shapes plant a 2-byte address placeholder that the linker
/// resolves via a GlobalAddr fixup.
fn emit_assign_global(global_idx: usize, value: &Expr, locals: &[Option<i32>], out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Some(k) = value.fold(locals) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&imm.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0xA3);                       // MOV moffs16, AX
        let addr_off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
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
    frame: Frame,
    return_int: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    emit_loop(cond, &[body_stmt], locals, frame, return_int, out, fixups);
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
    frame: Frame,
    return_int: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    emit_stmt(init, locals, frame, return_int, out, fixups);
    // The looped section is `step; body;` — treated as a single
    // "loop body" for the shared shape helper.
    emit_loop(cond, &[step, body_stmt], locals, frame, return_int, out, fixups);
}

/// Shared loop emitter — handles the alignment-pad, body
/// concatenation, cmp+jcc tail, and backward-jcc displacement.
/// Both while-loops (single-body) and for-loops (step+body) route
/// through here.
fn emit_loop(
    cond: &Cond,
    body_segments: &[&Stmt],
    locals: &[Option<i32>],
    frame: Frame,
    return_int: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    let mut body_buf = Vec::new();
    let mut body_fixups: Vec<Fixup> = Vec::new();
    for seg in body_segments {
        emit_stmt(seg, locals, frame, return_int, &mut body_buf, &mut body_fixups);
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
    let body_base = out.len();
    out.extend_from_slice(&body_buf);
    for mut c in body_fixups {
        c.body_offset += body_base;
        fixups.push(c);
    }
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
    frame: Frame,
    return_int: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    let mut body_buf = Vec::new();
    let mut body_fixups: Vec<Fixup> = Vec::new();
    emit_stmt(body_stmt, locals, frame, return_int, &mut body_buf, &mut body_fixups);
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
    let body_base = out.len();
    out.extend_from_slice(&body_buf);
    for mut c in body_fixups {
        c.body_offset += body_base;
        fixups.push(c);
    }
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
    let Stmt::Assign { target: AssignTarget::Local(local_idx), value } = body else { return false };
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

/// Append the bytes that compute `expr` into AX. Caller has already
/// emitted the prologue + chkstk call; what we emit here lives
/// between the chkstk call and the return-path epilogue. Phase 1
/// supports a tight set of patterns — every other shape panics with
/// a clear message so the missing case is obvious when a future
/// fixture hits it.
fn emit_expr_to_ax(expr: &Expr, locals: &[Option<i32>], out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
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
        Expr::Param(i) => {
            emit_load_param(*i, out);
        }
        Expr::BinOp { op, left, right } => {
            emit_binop(*op, left, right, locals, out);
        }
        Expr::Call { name, .. } => {
            panic!("Call to `{name}` inside a non-return expression context not yet supported");
        }
        Expr::StrLit(_) => {
            panic!("string literal in non-arg context not yet supported");
        }
        Expr::Global(idx) => {
            // `a1 00 00` — mov ax, moffs16. The placeholder address
            // gets FIXUP'd to the global's _DATA-relative offset.
            // Fixtures 4104, 4106.
            let body_offset = out.len();
            out.extend_from_slice(&[0xA1, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *idx },
            });
        }
        Expr::Deref { ptr } => {
            // Phase 1: only `*<char-ptr-global>` is supported. The
            // pattern is `mov bx, [p]; mov al, [bx]; cbw`. The FIXUP
            // sits at the `[p]` address inside the mov-bx encoding.
            match ptr.as_ref() {
                Expr::Global(idx) => {
                    let body_offset = out.len();
                    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
                    fixups.push(Fixup {
                        body_offset: body_offset + 1,
                        kind: FixupKind::GlobalAddr { global_idx: *idx },
                    });
                    out.extend_from_slice(&[0x8A, 0x07, 0x98]);
                }
                other => panic!("deref of {other:?} not yet supported"),
            }
        }
        Expr::Index { array, index } => {
            if let Some(k) = index.fold(locals) {
                // Constant index → `a1 <byte_off>` with FIXUP. The
                // placeholder is `byte_off` (not zero); the linker
                // adds the array's base address. Fixture 4109.
                let byte_off = (k as u32).wrapping_mul(2) as u16;
                let body_offset = out.len();
                out.push(0xA1);
                out.extend_from_slice(&byte_off.to_le_bytes());
                fixups.push(Fixup {
                    body_offset,
                    kind: FixupKind::GlobalAddr { global_idx: *array },
                });
            } else {
                // Variable index → load it into BX, scale ×2 with
                // `shl bx, 1`, then `mov ax, [bx+addr]` with FIXUP.
                // Fixture 4112.
                match index.as_ref() {
                    Expr::Param(i) => {
                        let disp = i8::try_from(4 + (*i * 2))
                            .expect("param disp fits");
                        out.extend_from_slice(&[0x8B, 0x5E, disp as u8]);
                    }
                    Expr::Local(i) => {
                        let disp = -(i16::try_from(*i + 1)
                            .expect("local idx") * 2);
                        out.extend_from_slice(&[0x8B, 0x5E, disp as u8]);
                    }
                    other => panic!(
                        "non-const, non-param/local array index not supported: {other:?}"
                    ),
                }
                out.extend_from_slice(&[0xD1, 0xE3]);
                let body_offset = out.len();
                out.extend_from_slice(&[0x8B, 0x87, 0x00, 0x00]);
                fixups.push(Fixup {
                    body_offset: body_offset + 1,
                    kind: FixupKind::GlobalAddr { global_idx: *array },
                });
            }
        }
    }
}

/// `mov ax, word ptr [bp + 4 + 2*idx]` — load a parameter into AX.
/// Same `8B 46 disp8` form as locals, just with a positive
/// displacement. Fixture 4102 (`return a + b;`) exercises this.
fn emit_load_param(idx: usize, out: &mut Vec<u8>) {
    let disp = i8::try_from(4 + (idx * 2)).expect("param disp fits in i8");
    out.push(0x8B);
    out.push(0x46);
    out.push(disp as u8);
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
    // Left as a BP-rel operand we can load into AX.
    if let Some(load) = bp_load(left) {
        load(out);
        // Right as IntLit → imm form.
        if let Expr::IntLit(k) = right {
            emit_imm_op(op, *k, out);
            return;
        }
        // Right as BP-rel → `op ax, [bp+disp]` mem form.
        if let Some(disp) = bp_disp(right) {
            emit_mem_op_at(op, disp, out);
            return;
        }
    }
    // Foldable side — recurse with the folded literal substituted.
    // Lets `(2 + x)` collapse to `(<lit> + <local>)` etc.
    if let Some(k) = left.fold(locals) {
        emit_binop(op, &Expr::IntLit(k), right, locals, out);
        return;
    }
    if let Some(k) = right.fold(locals) {
        emit_binop(op, left, &Expr::IntLit(k), locals, out);
        return;
    }
    panic!("binop shape not yet supported: {op:?} of {left:?}, {right:?}");
}

/// If `e` is a Local or Param, return a closure that emits the
/// `mov ax, [bp+disp]` load. Otherwise return None. Used by
/// `emit_binop` to handle either operand kind on the left-hand side.
fn bp_load(e: &Expr) -> Option<Box<dyn FnOnce(&mut Vec<u8>) + '_>> {
    match e {
        Expr::Local(i) => Some(Box::new(move |out: &mut Vec<u8>| emit_load_local(*i, out))),
        Expr::Param(i) => Some(Box::new(move |out: &mut Vec<u8>| emit_load_param(*i, out))),
        _ => None,
    }
}

/// If `e` is a Local or Param, return its bp-relative byte
/// displacement (negative for locals, positive for params).
fn bp_disp(e: &Expr) -> Option<i16> {
    match e {
        Expr::Local(i) => Some(-(*i as i16 + 1) * 2),
        Expr::Param(i) => Some(4 + (*i as i16) * 2),
        _ => None,
    }
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

/// Per-operator emit for `<reg-AX> <op> word ptr [bp+disp]`. The
/// opcode-prefix byte for memory-source forms: 03=ADD, 2B=SUB.
/// Works for both negative disps (locals) and positive disps
/// (params); fixture 4102 uses param shape.
fn emit_mem_op_at(op: BinOp, disp: i16, out: &mut Vec<u8>) {
    let opcode = match op {
        BinOp::Add => 0x03,
        BinOp::Sub => 0x2B,
        BinOp::Mul => panic!("memory-source mul not yet covered by a fixture"),
    };
    let disp8 = i8::try_from(disp).expect("disp fits in i8");
    out.push(opcode);
    out.push(0x46);  // ModR/M: mod=01 (disp8), reg=000 (AX), r/m=110 (BP-rel)
    out.push(disp8 as u8);
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
