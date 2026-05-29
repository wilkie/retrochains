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

/// A translation unit: a sequence of function definitions in
/// source order. Phase 1 only handles non-static cdecl functions —
/// every entry contributes both a PUBDEF and (per MSC's habit) an
/// EXTDEF declaration.
#[derive(Debug, Clone)]
pub struct Unit {
    pub functions: Vec<Function>,
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
    Comma,
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
    param_names: Vec<String>,
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
    };
    let mut functions = Vec::new();
    while p.peek().is_some() {
        functions.push(parse_function(&mut p)?);
    }
    if functions.is_empty() {
        return Err(EmitError::Unsupported(
            "translation unit has no functions".to_owned(),
        ));
    }
    Ok(Unit { functions })
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
/// list of (offset, target-name) call sites that need their `e8
/// disp16` placeholders patched after we know each function's global
/// position within the combined LEDATA.
struct FunctionEmit {
    bytes: Vec<u8>,
    calls: Vec<CallSite>,
    chkstk_offset_in_body: usize,
}

#[derive(Debug)]
struct CallSite {
    /// Offset of the `e8` opcode within `FunctionEmit::bytes`.
    body_offset: usize,
    /// Name of the function being called.
    target: String,
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
    for (i, fe) in function_emits.iter().enumerate() {
        function_offsets.push(cursor);
        offset_by_name.insert(symbol_name(&unit.functions[i].name), cursor);
        cursor += fe.bytes.len();
    }
    let total_code_bytes = cursor;
    let text_len = u16::try_from(total_code_bytes).expect("_TEXT body fits in u16");

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

    // EXTDEF — external symbols. MSC always includes:
    //   __acrtused  — sentinel forcing LINK to pull the C runtime
    //                 startup. Type-idx 0x01 marks it special.
    //   __chkstk    — stack checker, called from every prologue.
    // Then every PUBDEF in the OBJ also gets declared here, in
    // source order. Fixture 4099 confirms (`_f` and `_main`).
    {
        let mut payload = Vec::new();
        for (name, ty) in [("__acrtused", 0x01u8), ("__chkstk", 0x00)] {
            payload.push(u8::try_from(name.len()).expect("EXTDEF name fits"));
            payload.extend_from_slice(name.as_bytes());
            payload.push(ty);
        }
        for f in &unit.functions {
            let sym = symbol_name(&f.name);
            payload.push(u8::try_from(sym.len()).expect("EXTDEF name fits"));
            payload.extend_from_slice(sym.as_bytes());
            payload.push(0);
        }
        b.write_record(obj::EXTDEF, &payload);
    }

    // PUBDEF — every function in the unit, at its global offset.
    // MSC packs all functions into one PUBDEF record (fixture
    // 4099). `write_pubdef16` only handles a single name, so build
    // the multi-entry payload manually.
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

    // Patch all intra-segment calls. For each call site, compute the
    // self-relative 16-bit displacement = target_offset -
    // (caller_function_offset + body_offset + 3). The 3 is the
    // length of the `e8 disp16` call instruction; rel16 is computed
    // from the byte AFTER the instruction.
    let mut function_emits = function_emits;
    for (i, fe) in function_emits.iter_mut().enumerate() {
        let caller_off = function_offsets[i];
        for call in &fe.calls {
            let target_off = offset_by_name
                .get(&call.target)
                .copied()
                .expect("forward refs / extern calls not yet supported");
            let disp = (target_off as i32)
                - (caller_off as i32 + call.body_offset as i32 + 3);
            let disp16 = (disp as i32 & 0xFFFF) as u16;
            fe.bytes[call.body_offset + 1] = (disp16 & 0xFF) as u8;
            fe.bytes[call.body_offset + 2] = ((disp16 >> 8) & 0xFF) as u8;
        }
    }

    // Concatenate every function's body into the single LEDATA
    // payload at _TEXT offset 0.
    let mut all_code = Vec::with_capacity(total_code_bytes);
    for fe in &function_emits {
        all_code.extend_from_slice(&fe.bytes);
    }
    b.write_ledata16(1, 0, &all_code);

    // FIXUPP — patch each function's `call __chkstk`. The patch
    // offset is the LEDATA-relative position of the disp bytes of
    // the call instruction, i.e. function_global_offset +
    // chkstk_offset_in_body.
    let mut fixup_payload = Vec::new();
    // Emit FIXUPs in descending offset order — fixture 4099 shows
    // MSC sorts later-offset patches first (the 4099 FIXUPP is
    // `84 09 56 02  84 03 56 02`, with offset 9 before offset 3).
    let mut chkstk_patch_offsets: Vec<u8> = function_emits
        .iter()
        .enumerate()
        .map(|(i, fe)| {
            u8::try_from(function_offsets[i] + fe.chkstk_offset_in_body)
                .expect("chkstk patch offset fits in u8")
        })
        .collect();
    chkstk_patch_offsets.sort_by(|a, b| b.cmp(a));
    for off in &chkstk_patch_offsets {
        fixup_payload.extend_from_slice(&[0x84, *off, 0x56, 0x02]);
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
fn emit_function(func: &Function) -> FunctionEmit {
    let mut bytes = Vec::with_capacity(32);
    let mut calls: Vec<CallSite> = Vec::new();
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
    // Position the chkstk e8's displacement bytes — the FIXUPP
    // patches them at link time. `bytes.len()` here is the offset
    // of the e8 opcode; the disp16 sits at `+1`.
    bytes.push(0xE8);
    let chkstk_offset_in_body = bytes.len();
    bytes.extend_from_slice(&[0x00, 0x00]);

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
    for stmt in &func.body {
        if !reachable {
            break;
        }
        emit_stmt(
            stmt,
            &func.locals,
            frame,
            func.return_int,
            &mut bytes,
            &mut calls,
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

    FunctionEmit { bytes, calls, chkstk_offset_in_body }
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
    calls: &mut Vec<CallSite>,
) {
    match stmt {
        Stmt::Return(expr) => emit_return(expr, locals, frame, return_int, out, calls),
        Stmt::Empty => {}
        Stmt::ExprStmt(Expr::Call { name, args }) => {
            emit_call(name, args, locals, out, calls);
        }
        Stmt::ExprStmt(other) => {
            panic!("ExprStmt with non-call expression not yet supported: {other:?}");
        }
        Stmt::Assign { local_idx, value } => emit_assign(*local_idx, value, locals, out),
        Stmt::While { cond, body } => {
            emit_while(cond, body, locals, frame, return_int, out, calls);
        }
        Stmt::DoWhile { body, cond } => {
            emit_do_while(body, cond, locals, frame, return_int, out, calls);
        }
        Stmt::For { init, cond, step, body } => {
            emit_for(init, cond, step, body, locals, frame, return_int, out, calls);
        }
        Stmt::If { cond, then_branch, else_branch } => {
            // Constant-condition elision: when the cond folds to a
            // compile-time integer, MSC keeps only the live branch
            // and drops the comparison + jump entirely. Fixtures
            // 4094 (if (0)) and 4095 (if (1)) confirm.
            if let Some(k) = fold_cond(cond, locals) {
                if k != 0 {
                    emit_stmt(then_branch, locals, frame, return_int, out, calls);
                } else if let Some(else_branch) = else_branch {
                    emit_stmt(else_branch, locals, frame, return_int, out, calls);
                }
                return;
            }
            // Build the then-branch into a scratch buffer so we know
            // its byte count for the conditional-jump displacement.
            let mut then_buf = Vec::new();
            let mut then_calls = Vec::new();
            emit_stmt(then_branch, locals, frame, return_int, &mut then_buf, &mut then_calls);
            let then_len = then_buf.len();
            let take_then_disp = i8::try_from(then_len)
                .expect("then-body short enough for jcc rel8");
            emit_cond_skip(cond, take_then_disp, out);
            // Bring any then-branch call sites into the parent buffer,
            // offsetting their body_offset by where the then bytes
            // land in `out`.
            let then_base = out.len();
            out.extend_from_slice(&then_buf);
            for mut c in then_calls {
                c.body_offset += then_base;
                calls.push(c);
            }
            if let Some(else_branch) = else_branch {
                emit_stmt(else_branch, locals, frame, return_int, out, calls);
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
    calls: &mut Vec<CallSite>,
) {
    if return_int {
        // Return-of-call peephole: `return f(args);` leaves the
        // result in AX from the call's return value — no extra
        // load before ret. Fixture 4102 confirms.
        if let Expr::Call { name, args } = expr {
            emit_call(name, args, locals, out, calls);
        } else if let Some(k) = expr.fold(locals) {
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
    calls: &mut Vec<CallSite>,
) {
    for arg in args.iter().rev() {
        emit_push_arg(arg, locals, out);
    }
    let body_offset = out.len();
    out.extend_from_slice(&[0xE8, 0x00, 0x00]);
    calls.push(CallSite { body_offset, target: symbol_name(name) });
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
/// via `mov ax, K; push ax`; locals/params via direct memory push.
fn emit_push_arg(arg: &Expr, _locals: &[Option<i32>], out: &mut Vec<u8>) {
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
        other => panic!("argument shape not yet supported: {other:?}"),
    }
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
    frame: Frame,
    return_int: bool,
    out: &mut Vec<u8>,
    calls: &mut Vec<CallSite>,
) {
    emit_loop(cond, &[body_stmt], locals, frame, return_int, out, calls);
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
    calls: &mut Vec<CallSite>,
) {
    emit_stmt(init, locals, frame, return_int, out, calls);
    // The looped section is `step; body;` — treated as a single
    // "loop body" for the shared shape helper.
    emit_loop(cond, &[step, body_stmt], locals, frame, return_int, out, calls);
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
    calls: &mut Vec<CallSite>,
) {
    let mut body_buf = Vec::new();
    let mut body_calls: Vec<CallSite> = Vec::new();
    for seg in body_segments {
        emit_stmt(seg, locals, frame, return_int, &mut body_buf, &mut body_calls);
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
    for mut c in body_calls {
        c.body_offset += body_base;
        calls.push(c);
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
    calls: &mut Vec<CallSite>,
) {
    let mut body_buf = Vec::new();
    let mut body_calls: Vec<CallSite> = Vec::new();
    emit_stmt(body_stmt, locals, frame, return_int, &mut body_buf, &mut body_calls);
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
    for mut c in body_calls {
        c.body_offset += body_base;
        calls.push(c);
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

/// Append the bytes that compute `expr` into AX. Caller has already
/// emitted the prologue + chkstk call; what we emit here lives
/// between the chkstk call and the return-path epilogue. Phase 1
/// supports a tight set of patterns — every other shape panics with
/// a clear message so the missing case is obvious when a future
/// fixture hits it.
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
        Expr::Param(i) => {
            emit_load_param(*i, out);
        }
        Expr::BinOp { op, left, right } => {
            emit_binop(*op, left, right, locals, out);
        }
        Expr::Call { name, .. } => {
            panic!("Call to `{name}` inside a non-return expression context not yet supported");
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
