//! Hi-IR: expression and statement recovery — §5 of `specs/decompiler/IR.md`.
//!
//! Two recoveries live here. **Expression folding** symbolically executes the
//! `ax` accumulator across a straight-line run — BCC evaluates each expression
//! into `ax` and stores it back, never keeping a value live across a statement,
//! so `Load(ax,b)`, `Bin(ax,+,ax,c)`, `Store(a,ax)` collapses to `a = b + c`.
//! **Control-flow structuring** then recovers `if`/`while` by pattern-matching
//! BCC's stereotyped branch shapes rather than doing general interval analysis:
//!
//! - an `if` is a forward `cmp` + conditional branch that *skips* the then-block
//!   (so the source condition is the branch condition **negated**); an `if/else`
//!   adds an unconditional jump at the then-block's tail skipping the else;
//! - a `while` is loop-rotated: an unconditional jump to a bottom test that
//!   conditionally branches **back** to the body (the branch condition is the
//!   loop-continue condition, *not* negated).
//!
//! This increment recovers straight-line and `if`/`while` functions over `int`
//! **stack** locals (register-variable data-flow — BCC's default `si`/`di`
//! allocation — is a separate follow-up). Anything it can't model sets
//! [`Function::complete`] to `false`, so [`crate::decompile`] returns `None`
//! rather than present a half-recovery; the recompile-verify loop adjudicates
//! the rest.

use crate::lo_ir::{lift, BinOp, ByteReg, Cond, LoInsn, LoOp, Place, Promote, Reg, UnOp};

/// A relational operator recovered from a `cmp` + conditional branch. The
/// unsigned variants (from `jb`/`ja`/…) print the same C token as their signed
/// peers but mark their operands `unsigned` (so the compare re-emits as `jb`/`ja`
/// rather than `jl`/`jg`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    ULt,
    ULe,
    UGt,
    UGe,
}

impl RelOp {
    /// The opposite test — used to invert a branch that *skips* a block.
    fn negate(self) -> RelOp {
        match self {
            RelOp::Eq => RelOp::Ne,
            RelOp::Ne => RelOp::Eq,
            RelOp::Lt => RelOp::Ge,
            RelOp::Ge => RelOp::Lt,
            RelOp::Le => RelOp::Gt,
            RelOp::Gt => RelOp::Le,
            RelOp::ULt => RelOp::UGe,
            RelOp::UGe => RelOp::ULt,
            RelOp::ULe => RelOp::UGt,
            RelOp::UGt => RelOp::ULe,
        }
    }

    /// Is this an unsigned comparison (its operands are `unsigned`)?
    fn is_unsigned(self) -> bool {
        matches!(self, RelOp::ULt | RelOp::ULe | RelOp::UGt | RelOp::UGe)
    }
}

/// The relational operator a `Jcc` low-nibble condition tests, or `None` for the
/// codes this increment doesn't model (unsigned and the flag-only jumps). The
/// branch is *taken* when this holds.
fn cond_to_relop(cond: Cond) -> Option<RelOp> {
    match cond.0 {
        0x4 => Some(RelOp::Eq),  // jz / je
        0x5 => Some(RelOp::Ne),  // jnz / jne
        0xc => Some(RelOp::Lt),  // jl
        0xd => Some(RelOp::Ge),  // jge
        0xe => Some(RelOp::Le),  // jle
        0xf => Some(RelOp::Gt),  // jg
        0x2 => Some(RelOp::ULt), // jb / jc
        0x3 => Some(RelOp::UGe), // jae / jnb
        0x6 => Some(RelOp::ULe), // jbe / jna
        0x7 => Some(RelOp::UGt), // ja / jnbe
        _ => None,               // jo/js/jp need more context
    }
}

/// A prefix unary operator that maps directly to one machine op (distinct from
/// the logical `!`, which is the `neg;sbb;inc` idiom — [`Expr::Not`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-e` — arithmetic negation (`neg`).
    Neg,
    /// `~e` — bitwise complement (`not`).
    BitNot,
}

/// A recovered expression — a subset of the §5 grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// An integer constant.
    Const(i32),
    /// A 32-bit `long` constant (emitted with an `L` suffix so a value outside
    /// `int` range isn't read as `unsigned int` and truncated).
    LongConst(i32),
    /// A local variable — a stack slot, parameter, or register variable.
    Var(Var),
    /// `lhs op rhs` (left-associative, as the accumulator chain produces).
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `lhs rel rhs` — a comparison (the condition of an `if`/`while`).
    Rel(RelOp, Box<Expr>, Box<Expr>),
    /// `!e` — logical negation, recovered from the `or r,r; jnz` truthiness test.
    Not(Box<Expr>),
    /// `*e` — a pointer dereference (`mov bx,p; mov ax,[bx]`).
    Deref(Box<Expr>),
    /// `&v` — the address of a variable (`lea ax,[bp+disp]`).
    AddrOf(Var),
    /// A call with its callee's `_TEXT` byte offset and argument list (source
    /// order). The offset is the near-call target: in a multi-function program it
    /// names a *local* callee (the call lands on its prologue); an external
    /// callee is a placeholder `e8 00 00` whose target matches no function start,
    /// so it stays an opaque extern (any declared extern reproduces those bytes —
    /// only the argument count/types matter).
    Call(usize, Vec<Expr>),
    /// `(ty)e` — a narrowing cast. Recovered when an `int` value is stored to a
    /// `char` via its low byte (`mov al,[x]; mov [c],al`): a plain `c = x` would
    /// re-evaluate `x` at word width (`mov ax,[x]`), so the cast is what
    /// reproduces the byte load.
    Cast(Type, Box<Expr>),
    /// `-e` / `~e` — a prefix unary op (`neg`/`not`). Logical `!` is [`Not`].
    Unary(UnaryOp, Box<Expr>),
    /// `c ? t : f` — a conditional expression. Recovered from the diamond
    /// `test; jcc else; <t→ax>; jmp end; else: <f→ax>; end:` where both arms
    /// reduce to a single accumulator value (no statements).
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `*p++` / `*p--` — dereference a pointer, then post-increment/decrement it.
    /// Recovered from the BCC idiom `mov bx,p; inc/dec p (×stride); mov ax,[bx]`
    /// (the old pointer saved in `bx`, advanced in place, then deref'd). `true`
    /// is the decrement form.
    PostIncDeref(Var, bool),
}

/// Does an expression contain a (side-effecting) call anywhere?
fn contains_call(e: &Expr) -> bool {
    match e {
        Expr::Call(..) => true,
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => contains_call(a) || contains_call(b),
        Expr::Ternary(a, b, c) => contains_call(a) || contains_call(b) || contains_call(c),
        Expr::Not(a) | Expr::Deref(a) | Expr::Cast(_, a) | Expr::Unary(_, a) => contains_call(a),
        Expr::Const(_) | Expr::LongConst(_) | Expr::Var(_) | Expr::AddrOf(_) | Expr::PostIncDeref(..) => false,
    }
}

/// Is an accumulator value safe to fold as a compound's right-hand side
/// (`x op= <e>`)? Plain arithmetic over variables and constants is; a memory
/// load (`Deref`/`PostIncDeref`) or a `Call` is not — those expose unrelated
/// gaps when completing the function (a deref RHS is the later deref stage; a
/// call RHS hits the `push si` arg/save ambiguity), so they stay declined.
fn is_simple_compound_rhs(e: &Expr) -> bool {
    match e {
        Expr::Const(_) | Expr::LongConst(_) | Expr::Var(_) | Expr::AddrOf(_) => true,
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => {
            is_simple_compound_rhs(a) && is_simple_compound_rhs(b)
        }
        Expr::Ternary(a, b, c) => {
            is_simple_compound_rhs(a) && is_simple_compound_rhs(b) && is_simple_compound_rhs(c)
        }
        Expr::Not(a) | Expr::Cast(_, a) | Expr::Unary(_, a) => is_simple_compound_rhs(a),
        Expr::Deref(_) | Expr::PostIncDeref(..) | Expr::Call(..) => false,
    }
}

/// A recovered local variable. BCC keeps `int` locals either on the stack frame
/// or in the `si`/`di` register variables; both lift to ordinary named locals
/// (the storage class is a hint, not semantics — and recompiling a plain `int`
/// reproduces BCC's deterministic register allocation, so the emitter doesn't
/// even mark them `register`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Var {
    /// A stack slot at `[bp+disp]` (`disp < 0`).
    Slot(i16),
    /// A register variable (`si` or `di`).
    Reg(Reg),
    /// A parameter at `[bp+disp]` (`disp ≥ 4`, past the saved bp and return
    /// address). In the small model the first parameter is `[bp+4]`.
    Param(i16),
    /// A near global, identified by its offset within the data segment. Unlike a
    /// call target, this offset is *not* a placeholder — it's the real
    /// DGROUP-relative displacement the linker keeps, so reproducing it means
    /// declaring globals in the same order to get the same offsets.
    Global(u16),
    /// A `char` register variable (`dl`, `bl`, …) — the byte analogue of a
    /// [`Var::Reg`]. Always `char`.
    ByteReg(ByteReg),
}

/// An assignable location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LValue {
    /// A local variable.
    Var(Var),
    /// `*p` — a store through a pointer.
    Deref(Box<Expr>),
}

/// What the `dx` register holds while a 32-bit `long` is being assembled (its
/// high word): a constant (`xor dx,dx` → 0, or `mov dx,imm`), or the slot above a
/// `long` variable's low word (`mov dx,[lo+2]`).
#[derive(Clone, Copy)]
enum DxState {
    Const(i32),
    High(i16),
}

/// Is `r` one of the registers BCC uses for `int` register variables?
fn is_reg_var(r: Reg) -> bool {
    matches!(r, Reg::Si | Reg::Di)
}

/// Is `r` a byte register BCC uses for a `char` register variable? `al`/`ah` are
/// excluded — `ax` is the accumulator, not a variable.
fn is_byte_reg_var(r: ByteReg) -> bool {
    matches!(r, ByteReg::Cl | ByteReg::Dl | ByteReg::Bl | ByteReg::Ch | ByteReg::Dh | ByteReg::Bh)
}

/// A recovered statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `lvalue = expr;`
    Assign(LValue, Expr),
    /// `lvalue op= expr;` — an *in-place* modification (`x += 5`, `g++`). BCC
    /// codes this distinctly from `lvalue = lvalue op expr`: a register variable
    /// or memory operand is updated in a single instruction (`add si,5`,
    /// `inc word [g]`), not via a load-op-store through the accumulator. The two
    /// recompile to different bytes, so the in-place byte form recovers here and
    /// the load-op-store form stays an [`Assign`](Stmt::Assign).
    Compound(LValue, BinOp, Expr),
    /// `return expr;` (or `return;` when the accumulator holds no value).
    Return(Option<Expr>),
    /// `expr;` — an expression evaluated for its side effect (a discarded call).
    ExprStmt(Expr),
    /// `if (cond) { then } else { otherwise }` — `otherwise` empty if no `else`.
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    /// `while (cond) { body }`.
    While(Expr, Vec<Stmt>),
    /// `do { body } while (cond);` — a backward branch with no header jump (the
    /// body runs at least once).
    Do(Expr, Vec<Stmt>),
    /// `for (init; cond; step) { body }` — recovered from a `while` whose loop
    /// variable is initialized just before it and stepped at the body's tail
    /// (BCC lowers `for` to exactly that shape).
    For(Box<Stmt>, Expr, Box<Stmt>, Vec<Stmt>),
    /// `switch (scrutinee) { case K: body … [default: body] }` — recovered from a
    /// compare-chain (`cmp ax,K; je case`)* or a jump table. The third field is
    /// the `default:` body when the no-match block ends in a `break` (otherwise
    /// empty — a `default:` that returns, or no default, is the post-switch code).
    Switch(Expr, Vec<(i32, Vec<Stmt>)>, Vec<Stmt>),
    /// `break;` — a `switch` case body ending in a jump to the post-switch code.
    Break,
}

/// Does `expr` mention variable `var`? Used to confirm a `for` loop variable.
fn expr_mentions(expr: &Expr, var: Var) -> bool {
    match expr {
        Expr::Var(v) | Expr::AddrOf(v) | Expr::PostIncDeref(v, _) => *v == var,
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => {
            expr_mentions(a, var) || expr_mentions(b, var)
        }
        Expr::Ternary(a, b, c) => {
            expr_mentions(a, var) || expr_mentions(b, var) || expr_mentions(c, var)
        }
        Expr::Not(a) | Expr::Deref(a) | Expr::Cast(_, a) | Expr::Unary(_, a) => expr_mentions(a, var),
        Expr::Call(_, args) => args.iter().any(|a| expr_mentions(a, var)),
        Expr::Const(_) | Expr::LongConst(_) => false,
    }
}

/// Render `while` loops with for-loop structure (a loop variable initialized
/// just before the loop and stepped at the body's tail) as `for` statements.
/// BCC lowers `for` and the equivalent `init; while { body; step }` to identical
/// code, so this is a faithful re-rendering — and the recompile check confirms it.
fn fold_for_loops(stmts: Vec<Stmt>) -> Vec<Stmt> {
    // Recurse into nested blocks first.
    let recursed = stmts.into_iter().map(|s| match s {
        Stmt::If(c, t, e) => Stmt::If(c, fold_for_loops(t), fold_for_loops(e)),
        Stmt::While(c, b) => Stmt::While(c, fold_for_loops(b)),
        Stmt::Do(c, b) => Stmt::Do(c, fold_for_loops(b)),
        Stmt::For(i, c, st, b) => Stmt::For(i, c, st, fold_for_loops(b)),
        Stmt::Switch(s, arms, def) => Stmt::Switch(
            s,
            arms.into_iter().map(|(k, b)| (k, fold_for_loops(b))).collect(),
            fold_for_loops(def),
        ),
        other => other,
    });

    let mut out: Vec<Stmt> = Vec::new();
    for s in recursed {
        let Stmt::While(cond, mut body) = s else {
            out.push(s);
            continue;
        };
        // The init is the preceding statement assigning the loop variable; the
        // step is the body's final statement assigning the same variable, which
        // must appear in the condition.
        // The step may be a plain assignment (`i = i + 1`, stack local) or an
        // in-place compound (`i++`, register variable) — both name the loop var.
        let assigned_var = |s: Option<&Stmt>| match s {
            Some(Stmt::Assign(LValue::Var(v), _) | Stmt::Compound(LValue::Var(v), _, _)) => Some(*v),
            _ => None,
        };
        let init_var = assigned_var(out.last());
        let step_var = assigned_var(body.last());
        // Require a real body beyond the step: a loop whose body is *only* the
        // step (`while (c < 9) { c = c + 1; }`) stays a `while` — an empty-body
        // `for` lowers differently, so converting it wouldn't round-trip.
        if let (Some(iv), Some(sv)) = (init_var, step_var)
            && iv == sv
            && body.len() >= 2
            && expr_mentions(&cond, iv)
        {
            let init = out.pop().expect("init present");
            let step = body.pop().expect("step present");
            out.push(Stmt::For(Box::new(init), cond, Box::new(step), body));
        } else {
            out.push(Stmt::While(cond, body));
        }
    }
    out
}

/// Apply `f` to every [`Var`] referenced in an expression (reads and `&v`).
fn walk_vars_expr(e: &mut Expr, f: &mut dyn FnMut(&mut Var)) {
    match e {
        Expr::Var(v) | Expr::AddrOf(v) | Expr::PostIncDeref(v, _) => f(v),
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => {
            walk_vars_expr(a, f);
            walk_vars_expr(b, f);
        }
        Expr::Ternary(cond, then_v, else_v) => {
            walk_vars_expr(cond, f);
            walk_vars_expr(then_v, f);
            walk_vars_expr(else_v, f);
        }
        Expr::Not(a) | Expr::Deref(a) | Expr::Cast(_, a) | Expr::Unary(_, a) => walk_vars_expr(a, f),
        Expr::Call(_, args) => args.iter_mut().for_each(|a| walk_vars_expr(a, f)),
        Expr::Const(_) | Expr::LongConst(_) => {}
    }
}

/// Apply `f` to every [`Var`] in an lvalue.
fn walk_vars_lvalue(lv: &mut LValue, f: &mut dyn FnMut(&mut Var)) {
    match lv {
        LValue::Var(v) => f(v),
        LValue::Deref(e) => walk_vars_expr(e, f),
    }
}

/// Apply `f` to every [`Var`] in a statement list (recursing into blocks).
fn walk_vars(stmts: &mut [Stmt], f: &mut dyn FnMut(&mut Var)) {
    for s in stmts {
        match s {
            Stmt::Assign(lv, e) | Stmt::Compound(lv, _, e) => {
                walk_vars_lvalue(lv, f);
                walk_vars_expr(e, f);
            }
            Stmt::Return(Some(e)) | Stmt::ExprStmt(e) => walk_vars_expr(e, f),
            Stmt::Return(None) | Stmt::Break => {}
            Stmt::If(c, t, el) => {
                walk_vars_expr(c, f);
                walk_vars(t, f);
                walk_vars(el, f);
            }
            Stmt::While(c, b) | Stmt::Do(c, b) => {
                walk_vars_expr(c, f);
                walk_vars(b, f);
            }
            Stmt::For(init, c, step, b) => {
                walk_vars(std::slice::from_mut(init), f);
                walk_vars_expr(c, f);
                walk_vars(std::slice::from_mut(step), f);
                walk_vars(b, f);
            }
            Stmt::Switch(sc, arms, def) => {
                walk_vars_expr(sc, f);
                for (_, b) in arms.iter_mut() {
                    walk_vars(b, f);
                }
                walk_vars(def, f);
            }
        }
    }
}

/// Count references to `target` across a statement list.
fn count_var(stmts: &mut [Stmt], target: Var) -> usize {
    let mut n = 0;
    walk_vars(stmts, &mut |v| {
        if *v == target {
            n += 1;
        }
    });
    n
}

/// An `int *` pointer's `p++` is `inc reg; inc reg` (stride 2), which the fold
/// recovers as two `Compound(p, +, 1)` — but one recovered `p++` already
/// recompiles to the two incs (pointer arithmetic scales by the pointee), so the
/// pair must collapse to one. Merge adjacent identical `±1` compounds on an
/// `int`-pointer register variable. (A `char *` is stride 1 — one inc per `++` —
/// so it's untouched.)
fn coalesce_int_pointer_increments(ctx: &Ctx, body: &mut Vec<Stmt>) {
    let is_int_ptr = |v: &Var| ctx.ptr_vars.contains(v) && !ctx.char_ptr_vars.contains(v);
    coalesce_inc_pairs(body, &is_int_ptr);
}

/// Walk `stmts` (and nested blocks), merging each adjacent pair of identical
/// `Compound(v, op, 1)` where `pred(v)` holds into a single statement.
fn coalesce_inc_pairs(stmts: &mut Vec<Stmt>, pred: &impl Fn(&Var) -> bool) {
    for s in stmts.iter_mut() {
        match s {
            Stmt::If(_, t, e) => {
                coalesce_inc_pairs(t, pred);
                coalesce_inc_pairs(e, pred);
            }
            Stmt::While(_, b) | Stmt::Do(_, b) | Stmt::For(_, _, _, b) => {
                coalesce_inc_pairs(b, pred);
            }
            Stmt::Switch(_, arms, def) => {
                for (_, b) in arms.iter_mut() {
                    coalesce_inc_pairs(b, pred);
                }
                coalesce_inc_pairs(def, pred);
            }
            _ => {}
        }
    }
    let is_unit_compound = |s: &Stmt| {
        matches!(s, Stmt::Compound(LValue::Var(v), op, Expr::Const(1))
            if matches!(op, BinOp::Add | BinOp::Sub) && pred(v))
    };
    let mut i = 0;
    while i + 1 < stmts.len() {
        if is_unit_compound(&stmts[i]) && stmts[i] == stmts[i + 1] {
            stmts.remove(i + 1);
        }
        i += 1;
    }
    // `p += K` on an `int *` is `add si, K*2` (the constant is a *byte* offset);
    // rescale it to the element count. The coalesced `++` is `Const(1)` (odd) and
    // is left alone — genuine byte offsets are even (stride 2).
    for s in stmts.iter_mut() {
        if let Stmt::Compound(LValue::Var(v), op, Expr::Const(k)) = s
            && matches!(op, BinOp::Add | BinOp::Sub)
            && pred(v)
            && *k % 2 == 0
        {
            *k /= 2;
        }
    }
}

/// Recover BCC's promotion of a parameter into a register variable. When a
/// parameter is mutated (or heavily used), BCC copies it into a register
/// variable at entry (`mov si,[bp+4]`) and works on that register — so the
/// recovery sees a leading `reg = param` and a fresh local. But the register
/// variable *is* the parameter: the slot is never re-read, so they can't
/// diverge. Rewrite the register back to the parameter and drop the copy, which
/// reproduces the same bytes (BCC re-promotes the mutated parameter) without the
/// spurious local — decisive for `char`, where the extra local would cost a
/// 2-byte frame, and cleaner for `int`.
///
/// Guard: only when the parameter appears *exactly once* (its copy), so the
/// substitution can't collide with an independent read of the same parameter.
fn promote_params(ctx: &mut Ctx, body: &mut Vec<Stmt>) {
    // Collect the leading `reg = param` copies (immutable scan).
    let mut candidates: Vec<(Var, Var)> = Vec::new();
    for s in body.iter() {
        match s {
            Stmt::Assign(LValue::Var(reg), Expr::Var(param))
                if matches!(reg, Var::Reg(_) | Var::ByteReg(_))
                    && matches!(param, Var::Param(_))
                    && !candidates.iter().any(|(r, _)| r == reg) =>
            {
                candidates.push((*reg, *param));
            }
            _ => break, // leading copies only
        }
    }
    // Keep the front-run whose parameter is read *only* in its copy, so the
    // rewrite can't collide with an independent read of the same parameter.
    let mut aliases: Vec<(Var, Var)> = Vec::new();
    for (reg, param) in candidates {
        if count_var(body, param) == 1 {
            aliases.push((reg, param));
        } else {
            break;
        }
    }
    if aliases.is_empty() {
        return;
    }
    body.drain(0..aliases.len());
    for (reg, param) in &aliases {
        walk_vars(body, &mut |v| {
            if *v == *reg {
                *v = *param;
            }
        });
        // Move the register variable's inferred type to the parameter, then drop
        // the register from the variable set (it's no longer a distinct local).
        for set in [
            &mut ctx.char_vars,
            &mut ctx.unsigned_vars,
            &mut ctx.long_vars,
            &mut ctx.ptr_vars,
            &mut ctx.char_ptr_vars,
        ] {
            if set.contains(reg) && !set.contains(param) {
                set.push(*param);
            }
            set.retain(|v| v != reg);
        }
        ctx.vars.retain(|v| v != reg);
    }
}

/// A recovered value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Int,
    /// A `char` return — the value is left in `al` (a byte) at the return, with
    /// no `cbw` widening it to `int`.
    Char,
    /// 32-bit `long` — held in `dx:ax` (high:low), two adjacent slots on the
    /// stack.
    Long,
    Void,
}

/// A recovered function body. The signature is intentionally thin: recompilation
/// doesn't depend on the function's *name* (it isn't in `_TEXT`), so we emit a
/// placeholder name and focus on the body and the slots it touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// The return type, inferred from whether any `return` carries a value.
    pub ret: Type,
    /// The local variables the body touches, in first-appearance order.
    pub vars: Vec<Var>,
    /// The subset of `vars` accessed at byte width — these are `char` (the rest
    /// are `int`). Width is inferred from the access, not guessed.
    pub char_vars: Vec<Var>,
    /// The subset of `vars` that are pointers (dereferenced somewhere) —
    /// declared `int *` rather than `int`.
    pub ptr_vars: Vec<Var>,
    /// The subset of pointers dereferenced at byte width — declared `char *`.
    /// Disjoint from `ptr_vars` (a `char *` is never also an `int *`).
    pub char_ptr_vars: Vec<Var>,
    /// The subset of `vars` that are 32-bit `long` (loaded as a `dx:ax` pair).
    pub long_vars: Vec<Var>,
    /// The subset of `vars` that are `unsigned` (compared with `jb`/`ja` or
    /// shifted logically with `shr`).
    pub unsigned_vars: Vec<Var>,
    /// Local arrays reconstructed from the frame. A constant array index folds
    /// to a direct `[bp+disp]` slot access, so an `int a[M]` looks like scalar
    /// slots; when those slots can't be the whole top-packed scalar layout the
    /// frame is modelled as an array instead (see [`recover`]).
    pub arrays: Vec<ArraySpec>,
    /// The recovered statements.
    pub body: Vec<Stmt>,
    /// `false` if any op couldn't be modelled — the function is only partially
    /// recovered and must not be presented as done.
    pub complete: bool,
    /// When `!complete`, the proximate cause — the signature of the op that
    /// defeated recovery (`op_sig`) or a structural tag (`structure:…`,
    /// `dangling-array`, …). `None` when `complete`. A triage aid; the emitter
    /// ignores it.
    pub bail_reason: Option<String>,
    /// The function's start offset in `_TEXT` (its prologue / `Enter`). In a
    /// multi-function program this is the offset a local `call` targets, so it
    /// names the callee; for a lone function it's just the segment start.
    pub start: usize,
}

/// A local array reconstructed from the stack frame. Element 0 sits at `base`
/// (the most-negative offset — BCC lays an array bottom-up), ascending by the
/// element stride toward `bp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayElem {
    /// `char` element — stride 1, byte access.
    Char,
    /// `int` element — stride 2, word access.
    Int,
    /// `long` element — stride 4, a `dx:ax` word pair.
    Long,
}

impl ArrayElem {
    /// The element width in bytes (the array's index scale).
    fn stride(self) -> i16 {
        match self {
            ArrayElem::Char => 1,
            ArrayElem::Int => 2,
            ArrayElem::Long => 4,
        }
    }

    /// The C type keyword.
    fn c_type(self) -> &'static str {
        match self {
            ArrayElem::Char => "char",
            ArrayElem::Int => "int",
            ArrayElem::Long => "long",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArraySpec {
    /// The `[bp+disp]` offset of element 0 (the array's lowest address).
    pub base: i16,
    /// The element count.
    pub len: u16,
    /// The element type (and thus the stride).
    pub elem: ArrayElem,
}

impl ArraySpec {
    /// The `bp` offset just past the last element (exclusive upper bound).
    fn end(self) -> i16 {
        self.base + self.elem.stride() * self.len.cast_signed()
    }

    /// The C type keyword for the element.
    pub(crate) fn c_type(self) -> &'static str {
        self.elem.c_type()
    }

    /// The element index a slot offset maps to, if it lies on this array. A
    /// `long` element owns two word slots (`lo` and `lo+2`); both map to the
    /// same element index.
    #[must_use]
    pub fn index_of(self, off: i16) -> Option<u16> {
        let s = self.elem.stride();
        if off >= self.base && off < self.end() {
            u16::try_from((off - self.base) / s).ok()
        } else {
            None
        }
    }
}

/// Swap `Add`↔`Sub` (for normalizing `x + (-K)` to `x - K`).
fn flip_addsub(op: BinOp) -> BinOp {
    match op {
        BinOp::Add => BinOp::Sub,
        _ => BinOp::Add,
    }
}

/// Strip the stride-scaling shift from a pointer index. BCC scales a variable
/// index to a byte offset (`i << log2(stride)`) before adding it to the pointer;
/// the C-level index is the un-shifted value `i`. A non-shift index (a `char *`,
/// stride 1, needs no scaling) is itself. The shift *amount* is trusted to match
/// the pointee — the recompile check is the gate.
fn strip_scale(mut index: Expr) -> (Expr, u8) {
    // Strip the whole scaling chain (`int` is `<<1`, `long` is `<<1 <<1`),
    // returning the count — the stride is `2^count`, which types the element
    // (1 shift → `int`, 2 → `long`) when the access width can't (a `long` array
    // store writes only the low word, so it has no `dx:ax` pair to read).
    let mut shifts = 0u8;
    while let Expr::Binary(BinOp::Shl, base, _amount) = index {
        index = *base;
        shifts += 1;
    }
    (index, shifts)
}

/// Build `lhs op rhs`, collapsing a constant shift applied to an existing
/// constant shift of the same kind: `(x >> a) >> b` ⇒ `x >> (a+b)`.
fn combine_shift(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
    if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Sar)
        && let Expr::Const(k) = rhs
        && let Expr::Binary(inner_op, base, inner_k) = &lhs
        && *inner_op == op
        && let Expr::Const(ik) = **inner_k
    {
        return Expr::Binary(op, base.clone(), Box::new(Expr::Const(ik + k)));
    }
    Expr::Binary(op, Box::new(lhs), Box::new(rhs))
}

/// The arithmetic ops the single-accumulator fold models as a C binary
/// expression. Flag-only (`cmp`/`test`), carry/borrow, rotates, and the `dx:ax`
/// multiplicatives are excluded — they belong to control flow or wider-than-int
/// code this increment doesn't recover.
fn is_foldable(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Add | BinOp::Sub | BinOp::Or | BinOp::And | BinOp::Xor | BinOp::Shl | BinOp::Shr | BinOp::Sar
    )
}

/// Flush a discarded side-effecting call from the accumulator as a statement,
/// leaving a non-call (pure) value in place untouched. Called at the points that
/// *discard* the accumulator — a fresh load, a new statement, a new argument
/// push, the run's end — but not at the transparent ops (`Cleanup`, the exit
/// `Jump`, `Leave`) that sit between a call and the use of its result.
fn flush_call(acc: &mut Option<Expr>, out: &mut Vec<Stmt>) {
    if acc.as_ref().is_some_and(contains_call)
        && let Some(e) = acc.take()
    {
        out.push(Stmt::ExprStmt(e));
    }
}

/// A recognized jump-table `switch` dispatch (see [`Ctx::detect_jump_table`]).
struct JumpTable {
    /// Index of the scrutinee load — where the dispatch begins.
    load_idx: usize,
    /// The scrutinee's source operand.
    scrut: Place,
    /// The scrutinee is a `char` (loaded as a byte and widened) rather than an
    /// `int`; `unsigned` if zero-extended (`mov ah,0`) rather than `cbw`.
    scrut_char: bool,
    scrut_unsigned: bool,
    /// The lowest case value (`dec`→1, `sub K`→K, none→0).
    base: i32,
    /// The highest index (the `cmp bx,N` value); `N+1` table entries.
    n: i32,
    /// The no-match (post-switch) block's byte offset.
    default_off: usize,
    /// The table's byte offset within `_TEXT`.
    table_disp: usize,
}

/// The structuring/folding context over one function's Lo-IR.
#[allow(clippy::struct_excessive_bools)] // independent recovery flags, not a state enum
struct Ctx {
    insns: Vec<LoInsn>,
    /// The raw `_TEXT` bytes — needed to read an embedded jump table (a `switch`
    /// dispatch reads `(N+1)` word offsets at a fixed displacement).
    code: Vec<u8>,
    vars: Vec<Var>,
    char_vars: Vec<Var>,
    ptr_vars: Vec<Var>,
    char_ptr_vars: Vec<Var>,
    long_vars: Vec<Var>,
    unsigned_vars: Vec<Var>,
    /// The low-word offsets of stack slots read back as a `dx:ax` pair
    /// (`mov dx,[lo+2]; mov ax,[lo]`) — i.e. genuine `long` locals. Computed up
    /// front so the store side can tell a `long` store *pair* (`[hi]=…;[lo]=…`)
    /// from two adjacent `int` stores, which are byte-identical at the store.
    long_slots: std::collections::HashSet<i16>,
    /// Word-accessed slot offsets and global addresses — an `int` (or wider). A
    /// byte load of such a place is reading the low byte of an `int`, so it must
    /// not be (mis-)typed `char`. Used to type the rhs of a `char op= int`.
    word_slots: std::collections::HashSet<i16>,
    word_globals: std::collections::HashSet<u16>,
    /// Local-array bases (`lea` offsets) dereferenced at byte width — `char`
    /// arrays. The element type of a purely variable-indexed array isn't in any
    /// slot, only in the byte-vs-word deref, so this carries that signal to the
    /// frame-layout pass.
    char_array_bases: Vec<i16>,
    /// Local-array bases dereferenced as a `dx:ax` word pair — `long` arrays.
    long_array_bases: Vec<i16>,
    complete: bool,
    returns_value: bool,
    returns_long: bool,
    returns_char: bool,
    /// A value to seed the *next* straight-line fold with — the merged result of
    /// a ternary diamond, which the following code (a `return`/store) consumes.
    pending_acc: Option<Expr>,
    /// Diagnostic: the index of the op `fold_linear` is currently processing, so a
    /// bail there can name the op that defeated it without allocating on the happy
    /// path. Read lazily by [`Ctx::cant`].
    cur: usize,
    /// Diagnostic: why recovery first set `complete = false` (the op signature or a
    /// structural tag). Surfaced as [`Function::bail_reason`] to drive triage; does
    /// not affect what is emitted.
    reason: Option<String>,
}

impl Ctx {
    /// Mark recovery incomplete, blaming the op `fold_linear` is on (`self.cur`).
    /// Keeps the *first* reason — the proximate cause — and is otherwise identical
    /// to setting `complete = false`.
    fn cant(&mut self) {
        self.complete = false;
        if self.reason.is_none() {
            let sig = self.insns.get(self.cur).map_or("?", |n| op_sig(&n.op));
            self.reason = Some(sig.to_string());
        }
    }

    /// Mark recovery incomplete with an explicit structural reason (a bail that
    /// isn't about one straight-line op — unrecognized control flow, a jump table
    /// we can't read, …).
    fn cant_for(&mut self, why: &str) {
        self.complete = false;
        if self.reason.is_none() {
            self.reason = Some(why.to_string());
        }
    }

    /// Record a variable the first time it's seen (preserving order). A byte
    /// register variable is always `char`.
    fn note(&mut self, var: Var) {
        if !self.vars.contains(&var) {
            self.vars.push(var);
        }
        if matches!(var, Var::ByteReg(_)) {
            self.note_char(var);
        }
    }

    /// Note a variable accessed at byte width — it's a `char`.
    fn note_char(&mut self, var: Var) {
        if !self.char_vars.contains(&var) {
            self.char_vars.push(var);
        }
    }

    /// The `*p++` / `*p--` idiom following a `mov bx, r` at index `i`: a run of
    /// in-place `inc`/`dec r` (the pointer advance, all one direction) then a word
    /// `mov ax,[bx]` dereferencing the saved old pointer. Returns `(deref index,
    /// is_decrement)`, or `None`. (The `char *` form defers the increment with no
    /// `bx` snapshot, so it doesn't match — and is recovered elsewhere.)
    fn post_inc_deref(&self, i: usize, r: Reg) -> Option<(usize, bool)> {
        let mut j = i + 1;
        let mut dec: Option<bool> = None;
        while let Some(LoOp::Un { dst: Place::Reg(d), op, operand: Place::Reg(o) }) =
            self.insns.get(j).map(|n| &n.op)
        {
            if *d != r || *o != r || !matches!(op, UnOp::Inc | UnOp::Dec) {
                break;
            }
            let this_dec = matches!(op, UnOp::Dec);
            if dec.is_some_and(|d| d != this_dec) {
                return None; // mixed inc/dec
            }
            dec = Some(this_dec);
            j += 1;
        }
        let dec = dec?; // need at least one advance
        matches!(
            self.insns.get(j).map(|n| &n.op),
            Some(LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::Deref(Reg::Bx) })
        )
        .then_some((j, dec))
    }

    /// Is register `r` dereferenced anywhere in the function (`[r]` / `[r+disp]`,
    /// load or store)? A reg var that is means it holds a pointer — so an
    /// immediate loaded into it is `&global`, not a literal.
    fn reg_is_dereferenced(&self, r: Reg) -> bool {
        self.insns.iter().any(|n| {
            let place = match &n.op {
                LoOp::Load { src, .. } => Some(src),
                // `Store`/`Bin` both dereference via `dst` — the latter is an
                // in-place compound (`add [si+disp],ax` = `*p op= y`).
                LoOp::Store { dst, .. } | LoOp::Bin { dst, .. } => Some(dst),
                _ => None,
            };
            matches!(place, Some(Place::Deref(d) | Place::DerefDisp(d, _)) if *d == r)
        })
    }

    /// Is a stack-local pointer? Its value is loaded into `bx` (`mov bx,[bp-slot]`)
    /// and `bx` is then dereferenced — the way BCC reads through a spilled `int *`.
    /// Distinguishes a slot assigned `&global` (a pointer) from one assigned a
    /// literal, so the address load recovers as `&g` rather than the bare offset.
    fn slot_is_dereferenced(&self, slot: i16) -> bool {
        let loaded_to_bx = self.insns.iter().any(|n| {
            matches!(&n.op, LoOp::Load { dst: Place::Reg(Reg::Bx), src: Place::Local(s) } if *s == slot)
        });
        loaded_to_bx && self.insns.iter().any(|n| {
            let place = match &n.op {
                LoOp::Load { src, .. } => Some(src),
                LoOp::Store { dst, .. } | LoOp::Bin { dst, .. } => Some(dst),
                _ => None,
            };
            matches!(place, Some(Place::Deref(Reg::Bx) | Place::DerefDisp(Reg::Bx, _)))
        })
    }

    /// Is `place` a word-accessed slot/global — an `int` (or wider), so a byte
    /// load of it is reading the low byte of an `int`, not a `char`?
    fn is_word_place(&self, place: Place) -> bool {
        match place {
            Place::Local(o) => self.word_slots.contains(&o),
            Place::Global(a) => self.word_globals.contains(&a),
            _ => false,
        }
    }

    /// If instruction `i+1` is the low-word half of a `long` register store
    /// paired with the high-word store at slot `s1` in register `r1`, return the
    /// low slot offset. The two stores write `dx:ax` to a `(lo, lo+2)` slot pair
    /// (high first), the registers being `ax`/`dx` in either order.
    fn paired_long_store_low(&self, i: usize, s1: i16, r1: Reg) -> Option<i16> {
        if !matches!(r1, Reg::Ax | Reg::Dx) {
            return None;
        }
        let LoOp::Store { dst: Place::Local(s2), src: Place::Reg(r2) } = self.insns.get(i + 1)?.op
        else {
            return None;
        };
        (matches!(r2, Reg::Ax | Reg::Dx) && r2 != r1 && s2 == s1 - 2).then_some(s2)
    }

    /// Re-type any word-accessed variable in `e` back to `int`: a byte load of an
    /// `int`'s low byte (the rhs of a `char op= int`) char-marks the slot, but
    /// the slot's word stores prove it's an `int`. Undoes that local mis-marking.
    fn untype_word_chars(&mut self, e: &Expr) {
        let mut drop = Vec::new();
        let mut probe = |v: &mut Var| {
            let word = match v {
                Var::Slot(o) | Var::Param(o) => self.word_slots.contains(o),
                Var::Global(a) => self.word_globals.contains(a),
                _ => false,
            };
            if word {
                drop.push(*v);
            }
        };
        // `walk_vars_expr` needs `&mut Expr`; this is a read-only probe, so clone.
        let mut tmp = e.clone();
        walk_vars_expr(&mut tmp, &mut probe);
        self.char_vars.retain(|v| !drop.contains(v));
    }

    /// Note a variable that's dereferenced — it's a pointer (`int *`).
    fn note_ptr(&mut self, var: Var) {
        if !self.ptr_vars.contains(&var) {
            self.ptr_vars.push(var);
        }
    }

    /// If `base` is a local-array element address `&a[0] + i` (an `AddrOf` of a
    /// slot, plus an index), record that array base as `char` — a byte deref of
    /// it is the only element-type evidence a variable-only-indexed array has.
    fn note_char_array_base(&mut self, base: &Expr) {
        if let Expr::Binary(BinOp::Add, lhs, _) = base
            && let Expr::AddrOf(Var::Slot(off)) = **lhs
            && !self.char_array_bases.contains(&off)
        {
            self.char_array_bases.push(off);
        }
    }

    /// As [`note_char_array_base`](Self::note_char_array_base), but for a `long`
    /// array — its element is read/written as a `dx:ax` word pair through the
    /// element address.
    fn note_long_array_base(&mut self, base: &Expr) {
        if let Expr::Binary(BinOp::Add, lhs, _) = base
            && let Expr::AddrOf(Var::Slot(off)) = **lhs
            && !self.long_array_bases.contains(&off)
        {
            self.long_array_bases.push(off);
        }
    }

    /// Is `ptr` the address of a `long` array element (`&a[0] + i` for a known
    /// `long` array)? A *store* through it writes only the low word (a BCC
    /// codegen quirk) — not a clean `long` assignment — so the recovery declines.
    fn is_long_array_elem(&self, ptr: &Expr) -> bool {
        if let Expr::Binary(BinOp::Add, lhs, _) = ptr
            && let Expr::AddrOf(Var::Slot(off)) = **lhs
        {
            self.long_array_bases.contains(&off)
        } else {
            false
        }
    }

    /// Note a pointer dereferenced at byte width — it's a `char *`.
    fn note_char_ptr(&mut self, var: Var) {
        if !self.char_ptr_vars.contains(&var) {
            self.char_ptr_vars.push(var);
        }
    }

    /// Note a variable loaded as a `dx:ax` pair — it's a 32-bit `long`.
    fn note_long(&mut self, var: Var) {
        if !self.long_vars.contains(&var) {
            self.long_vars.push(var);
        }
    }

    /// Mark every variable in `expr` as `unsigned` (an unsigned compare/shift).
    fn mark_unsigned(&mut self, expr: &Expr) {
        match expr {
            Expr::Var(v) => {
                if !self.unsigned_vars.contains(v) {
                    self.unsigned_vars.push(*v);
                }
            }
            Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => {
                self.mark_unsigned(a);
                self.mark_unsigned(b);
            }
            // The value branches carry the result's signedness; the condition
            // is a separate boolean.
            Expr::Ternary(_, b, c) => {
                self.mark_unsigned(b);
                self.mark_unsigned(c);
            }
            Expr::Not(a) | Expr::Deref(a) | Expr::Cast(_, a) | Expr::Unary(_, a) => self.mark_unsigned(a),
            Expr::Const(_) | Expr::LongConst(_) | Expr::AddrOf(_) | Expr::Call(..) | Expr::PostIncDeref(..) => {}
        }
    }

    /// Mark the variables directly compared at byte width as `char`.
    fn mark_char(&mut self, expr: &Expr) {
        match expr {
            Expr::Var(v) => self.note_char(*v),
            // A byte comparison on `*p` makes `p` a `char *`.
            Expr::Deref(inner) => {
                if let Expr::Var(v) = inner.as_ref() {
                    self.note_char_ptr(*v);
                }
            }
            _ => {}
        }
    }

    /// The `long` operand of an `add ax,lo`/`adc dx,hi` pair — a `long` variable
    /// (low at `lo`, high at `lo+2`) or a constant `(hi<<16)|lo`.
    fn long_operand(&mut self, lo: Place, hi: Place) -> Option<Expr> {
        match (lo, hi) {
            (Place::Local(l), Place::Local(h)) if h == l + 2 => {
                let v = Self::var_of(Place::Local(l))?;
                self.note(v);
                self.note_long(v);
                Some(Expr::Var(v))
            }
            (Place::Imm(lo), Place::Imm(hi)) => Some(Expr::LongConst((hi << 16) | (lo & 0xFFFF))),
            _ => None,
        }
    }

    /// A byte-width source operand: note the variable as `char` and return it.
    fn char_operand(&mut self, place: Place) -> Option<Expr> {
        let var = Self::var_of(place)?;
        self.note(var);
        self.note_char(var);
        Some(Expr::Var(var))
    }

    /// A byte-width destination: note the variable as `char` and return it.
    fn char_dest(&mut self, place: Place) -> Option<LValue> {
        let var = Self::var_of(place)?;
        self.note(var);
        self.note_char(var);
        Some(LValue::Var(var))
    }

    /// The variable a place names — a stack slot or a register variable — or
    /// `None` if it isn't a variable this increment models (a param, global,
    /// pointer, scratch register, …).
    fn var_of(place: Place) -> Option<Var> {
        match place {
            Place::Local(d) if d < 0 => Some(Var::Slot(d)),
            Place::Local(d) if d >= 4 => Some(Var::Param(d)),
            Place::Reg(r) if is_reg_var(r) => Some(Var::Reg(r)),
            // An even offset is a word (`int`) global; odd offsets (`char`
            // globals, struct/array interiors) aren't modelled yet.
            Place::Global(a) if a % 2 == 0 => Some(Var::Global(a)),
            // A byte register (other than the `al`/`ah` accumulator) is a `char`
            // register variable.
            Place::Byte(r) if is_byte_reg_var(r) => Some(Var::ByteReg(r)),
            _ => None,
        }
    }

    /// Is `op` expressible as a C compound-assignment (`+= -= &= |= ^=`)? These
    /// are the in-place ALU ops; `Cmp`/`Test` set flags and shifts use a
    /// different operand encoding, so they're excluded.
    fn is_compound_op(op: BinOp) -> bool {
        matches!(op, BinOp::Add | BinOp::Sub | BinOp::And | BinOp::Or | BinOp::Xor)
    }

    /// `ptr + k` (a pointer advanced by a constant *element* index), or just
    /// `ptr` when `k == 0`. The index is already scaled (the caller divides the
    /// byte displacement by the pointee stride).
    fn offset_ptr(ptr: Expr, k: i16) -> Expr {
        if k == 0 {
            ptr
        } else {
            Expr::Binary(BinOp::Add, Box::new(ptr), Box::new(Expr::Const(i32::from(k))))
        }
    }

    /// `*(ptr + k)` — a dereference at a constant element offset (`p[k]`).
    fn deref_at(ptr: Expr, k: i16) -> Expr {
        Expr::Deref(Box::new(Self::offset_ptr(ptr, k)))
    }

    /// Does `op` leave its result in `al` (a byte)? A `return` reached right
    /// after such an op — with no `cbw` between — is a `char` return, not `int`.
    fn writes_byte(op: &LoOp) -> bool {
        matches!(
            op,
            LoOp::Load { dst: Place::Byte(ByteReg::Al), .. }
                | LoOp::Un { dst: Place::Byte(ByteReg::Al), .. }
                | LoOp::Bin { dst: Place::Byte(ByteReg::Al), .. }
        )
    }

    /// The instruction index whose byte offset is `off` (first match), if any.
    fn idx_of(&self, off: usize) -> Option<usize> {
        self.insns.iter().position(|i| i.span.start == off)
    }

    /// Is `off` the start of the function epilogue? A jump there is a `return`,
    /// not an `if/else` skip or a loop edge — every `return <expr>` is
    /// `mov ax,val; jmp epilogue`. The epilogue begins with the register-variable
    /// restores (`pop si`/`di`), if any, then `Leave`/`Ret`; those ops appear
    /// nowhere else, so any is a valid epilogue target.
    fn is_epilogue(&self, off: usize) -> bool {
        self.insns.iter().any(|i| {
            i.span.start == off
                && matches!(i.op, LoOp::Leave | LoOp::Ret { .. } | LoOp::RestoreReg { .. })
        })
    }

    /// A source operand as an [`Expr`], or `None` if outside this increment (a
    /// param, global, scratch register, pointer, …).
    fn operand(&mut self, place: Place) -> Option<Expr> {
        if let Place::Imm(v) = place {
            return Some(Expr::Const(v));
        }
        let var = Self::var_of(place)?;
        self.note(var);
        Some(Expr::Var(var))
    }

    /// A destination operand as an [`LValue`], or `None` if unsupported yet.
    fn dest(&mut self, place: Place) -> Option<LValue> {
        let var = Self::var_of(place)?;
        self.note(var);
        Some(LValue::Var(var))
    }

    /// Fold a straight-line instruction range `[lo, hi)` into statements,
    /// symbolically tracking the accumulator. Any op it can't model marks the
    /// function incomplete.
    /// Returns the value left in the accumulator at the end of the run (e.g. the
    /// `*p` set up just before a `cmp` that reads `ax`) — `None` if nothing live.
    #[allow(clippy::too_many_lines)] // a flat per-op match reads better unsplit
    fn fold_linear(&mut self, lo: usize, hi: usize, out: &mut Vec<Stmt>) -> Option<Expr> {
        // Seed with a ternary result left pending by the structurer, if any.
        let mut acc: Option<Expr> = self.pending_acc.take();
        // The pointer value loaded into `bx` (for a `mov bx,p; mov ax,[bx]`
        // dereference).
        let mut bx: Option<Expr> = None;
        // The variable shift count loaded into `cl` (`mov cl,[y]; shl ax,cl`).
        // `cl` is the 8086 shift-count register, not a user variable here.
        let mut cl: Option<Expr> = None;
        // The high word of a `long` being assembled in `dx`, and whether the
        // current accumulator value is a `long` (its high word in `dx`).
        let mut dx: Option<DxState> = None;
        let mut acc_long = false;
        // After an `idiv`/`div`, `dx` holds the remainder of `(dividend, divisor)`
        // — a `mov ax,dx` that reads it recovers the `%` operator.
        let mut dx_rem: Option<(Expr, Expr)> = None;
        // A `long` add/sub in progress: the `add ax,lo` (op, low operand) waiting
        // for the `adc dx,hi` that completes it.
        let mut pending_long: Option<(BinOp, Place)> = None;
        // Arguments pushed for the call currently being assembled (push order).
        // Also serves the binary-operand *spill* (`push ax` … `pop ax`): the
        // left operand is pushed here and popped back when both sides have been
        // evaluated into registers.
        let mut pending_args: Vec<Expr> = Vec::new();
        // The right operand of a two-register binary op, saved in `dx` by a
        // `mov dx,ax` while the left is restored from the stack (`pop ax`); the
        // following `<op> ax,dx` combines them.
        let mut dx_temp: Option<Expr> = None;
        // A `long` element/pointee read in progress: `mov dx,[bx+2]` loaded the
        // high word, holding the element address; the following `mov ax,[bx]`
        // (low word) completes it into the accumulator as a `long`.
        let mut long_deref: Option<Expr> = None;
        // Each `return <expr>` is `mov ax,val; jmp epilogue`, so a jump to the
        // epilogue flushes the accumulator as a `Return`. Tracks whether the run
        // already returned that way, so the physical `Ret` it lands on isn't
        // double-counted (and a void fall-off — no such jump — returns nothing).
        let mut returned = false;
        // How many *following* instructions an arm already consumed, so the loop
        // skips them (a two-store `long` assignment skips 1; the `!x` idiom's
        // `sbb`/`inc` tail skips 2).
        let mut skip = 0usize;
        for i in lo..hi {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            self.cur = i; // blame this op if the arm below bails (see `cant`)
            match self.insns[i].op.clone() {
                // Frame setup/teardown carry no value. `cbw`/`cwd` are sign
                // promotions the accumulator already reflects — `cbw` is the
                // implicit `char`→`int`, `cwd` sets up `dx:ax` as the `idiv`
                // dividend (the fold's accumulator is that dividend).
                LoOp::Enter { .. }
                | LoOp::Leave
                | LoOp::SaveReg { .. }
                | LoOp::RestoreReg { .. }
                | LoOp::Cleanup { .. }
                | LoOp::Promote { kind: Promote::Cbw } => {}

                // `cwd` sign-extends `ax` into `dx:ax`. Feeding an `idiv` it just
                // sets up the dividend (a no-op — the accumulator already is it);
                // otherwise it widens an `int` to a `long`, `(long)i`, so the
                // accumulator is now a `long` value.
                LoOp::Promote { kind: Promote::Cwd } => {
                    let feeds_idiv = matches!(
                        self.insns.get(i + 1).map(|n| &n.op),
                        Some(LoOp::Bin { dst: Place::DxAx, op: BinOp::Idiv, .. })
                    );
                    if !feeds_idiv {
                        acc_long = true;
                    }
                }

                // A jump to the epilogue is a `return <accumulator>`. The return
                // *type* is `char` when the value is left in `al` (a byte) with no
                // `cbw` widening it — detectable locally as a byte-register write
                // immediately before the jump (a `cbw`, for an `int` return, would
                // be that instruction instead).
                LoOp::Jump { target } if self.is_epilogue(target) => {
                    let v = acc.take();
                    if v.is_some() {
                        self.returns_value = true;
                        if acc_long {
                            self.returns_long = true;
                        } else if i > lo && Self::writes_byte(&self.insns[i - 1].op) {
                            self.returns_char = true;
                        }
                    }
                    out.push(Stmt::Return(v));
                    returned = true;
                }
                // Any other jump in a straight-line run is unexpected (loop edges
                // and `if/else` skips are consumed by the structurer) — it falls
                // to the catch-all below and marks the function incomplete.

                // `push ax` — the accumulator is the argument; `push [bp+d]` or a
                // register push supplies a variable directly.
                LoOp::Arg { src: Place::Reg(Reg::Ax) } => match acc.take() {
                    Some(e) => pending_args.push(e),
                    None => self.cant(),
                },
                LoOp::Arg { src } => {
                    flush_call(&mut acc, out);
                    match self.operand(src) {
                        Some(e) => pending_args.push(e),
                        None => self.cant(),
                    }
                }

                // A call consumes the pending arguments (cdecl pushes them
                // right-to-left, so reverse to source order) and leaves its
                // result in the accumulator.
                LoOp::Call { target, .. } => {
                    let mut args = std::mem::take(&mut pending_args);
                    args.reverse();
                    acc = Some(Expr::Call(target, args));
                    // A call's result type isn't tracked; clear the `long` flag so
                    // a following `dx:ax` store of an *unrecovered* `long` (e.g. a
                    // `long` shift via a runtime helper) declines rather than
                    // folding a stale value.
                    acc_long = false;
                }

                // `mov dx, ax` — save the accumulator (a binary op's right
                // operand) into `dx` while the left is restored from the stack.
                LoOp::Load { dst: Place::Reg(Reg::Dx), src: Place::Reg(Reg::Ax) } => {
                    dx_temp.clone_from(&acc);
                }

                // `pop ax` — restore the spilled left operand of a binary op.
                LoOp::Pop { dst: Place::Reg(Reg::Ax) } => match pending_args.pop() {
                    Some(e) => acc = Some(e),
                    None => self.cant(),
                },

                LoOp::Ret { .. } => {
                    if !returned {
                        // Fell off the end (a void function) — any call left in
                        // the accumulator was a discarded statement, no value.
                        flush_call(&mut acc, out);
                        out.push(Stmt::Return(None));
                    }
                    // Otherwise the `return`s were emitted at the jumps to the
                    // epilogue; the physical `ret` they land on adds nothing.
                }

                // `mov al, [bx]` — dereference a `char *` (a byte load). The
                // pointer in bx is a `char *`; the value is a `char` (a following
                // `cbw` promoting it for an `int` context is the usual no-op).
                LoOp::Load { dst: Place::Byte(ByteReg::Al), src: Place::Deref(Reg::Bx) } => {
                    flush_call(&mut acc, out);
                    match bx.clone() {
                        Some(ptr) => {
                            if let Expr::Var(v) = ptr {
                                self.note_char_ptr(v);
                            }
                            self.note_char_array_base(&ptr);
                            acc = Some(Expr::Deref(Box::new(ptr)));
                        }
                        None => self.cant(),
                    }
                }

                // `mov dx, [bx+2]` — the high word of a `long` read through bx (a
                // `long` array element `a[i]`, or a `long *` deref). Holds the
                // element address until the low half (`mov ax,[bx]`) completes it.
                LoOp::Load { dst: Place::Reg(Reg::Dx), src: Place::DerefDisp(Reg::Bx, 2) } => {
                    long_deref.clone_from(&bx);
                    if long_deref.is_none() {
                        self.cant();
                    }
                }

                // `mov ax, [bx]` completing a `long` read started by the `[bx+2]`
                // high-word load above — the accumulator is the `long` element.
                LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::Deref(Reg::Bx) }
                    if long_deref.is_some() =>
                {
                    flush_call(&mut acc, out);
                    let ptr = long_deref.take().unwrap();
                    if let Expr::Var(v) = &ptr {
                        self.note_long(*v);
                    }
                    self.note_long_array_base(&ptr);
                    acc = Some(Expr::Deref(Box::new(ptr)));
                    acc_long = true;
                }

                // `mov ax, [bx]` — dereference the pointer held in bx (`*p`).
                LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::Deref(Reg::Bx) } => {
                    flush_call(&mut acc, out);
                    match bx.clone() {
                        Some(ptr) => {
                            if let Expr::Var(v) = ptr {
                                self.note_ptr(v);
                            }
                            acc = Some(Expr::Deref(Box::new(ptr)));
                        }
                        None => self.cant(),
                    }
                }

                // `mov ax, [si]` / `mov ax, [di]` — dereference a pointer held in a
                // register variable (`*p`). Unlike the bx form, the pointer *is*
                // the register variable, not a value loaded into a scratch.
                LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::Deref(r) }
                    if is_reg_var(r) =>
                {
                    flush_call(&mut acc, out);
                    let v = Var::Reg(r);
                    self.note(v);
                    self.note_ptr(v);
                    acc = Some(Expr::Deref(Box::new(Expr::Var(v))));
                }

                // `mov al, [si]` / `mov al, [di]` — deref a `char *` reg-var pointer.
                LoOp::Load { dst: Place::Byte(ByteReg::Al), src: Place::Deref(r) }
                    if is_reg_var(r) =>
                {
                    // `*p op= K` — char deref compound (AL-through): `mov al,[si];
                    // <op> al,K; mov [si],al`. Recover as a Compound, NOT the
                    // load-op-store Assign `*p = *p op K` (which would double the
                    // deref and mis-recompile). Constant rhs only.
                    if acc.is_none()
                        && let Some(LoOp::Bin {
                            dst: Place::Byte(ByteReg::Al),
                            op,
                            lhs: Place::Byte(ByteReg::Al),
                            rhs: Place::Imm(v),
                        }) = self.insns.get(i + 1).map(|n| n.op.clone())
                        && Self::is_compound_op(op)
                        && matches!(
                            self.insns.get(i + 2).map(|n| &n.op),
                            Some(LoOp::Store { dst: Place::Deref(r2), src: Place::Byte(_) }) if *r2 == r
                        )
                    {
                        let p = Var::Reg(r);
                        self.note(p);
                        self.note_char_ptr(p);
                        out.push(Stmt::Compound(
                            LValue::Deref(Box::new(Expr::Var(p))),
                            op,
                            Expr::Const(v),
                        ));
                        skip = 2;
                    } else {
                        flush_call(&mut acc, out);
                        let v = Var::Reg(r);
                        self.note(v);
                        self.note_char_ptr(v);
                        acc = Some(Expr::Deref(Box::new(Expr::Var(v))));
                    }
                }

                // `mov ax, [bx+disp]` — deref an `int *` at a constant byte
                // offset: `*(p + K)` where K = disp/2 (the `int` stride). An odd
                // displacement isn't a clean `int` index — bail.
                LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::DerefDisp(Reg::Bx, disp) } => {
                    flush_call(&mut acc, out);
                    match bx.clone() {
                        Some(ptr) if disp % 2 == 0 => {
                            if let Expr::Var(v) = ptr {
                                self.note_ptr(v);
                            }
                            acc = Some(Self::deref_at(ptr, disp / 2));
                        }
                        _ => self.cant(),
                    }
                }

                // `mov al, [bx+disp]` — deref a `char *` at a constant byte offset:
                // `*(p + K)` where K = disp (the `char` stride is 1).
                LoOp::Load { dst: Place::Byte(ByteReg::Al), src: Place::DerefDisp(Reg::Bx, disp) } => {
                    flush_call(&mut acc, out);
                    match bx.clone() {
                        Some(ptr) => {
                            if let Expr::Var(v) = ptr {
                                self.note_char_ptr(v);
                            }
                            acc = Some(Self::deref_at(ptr, disp));
                        }
                        None => self.cant(),
                    }
                }

                // `mov ax, [si+disp]` / `mov ax, [di+disp]` — deref a reg-var
                // `int *` at a constant offset: `*(p + K)`, K = disp/2 (an int
                // field at byte offset `disp`, e.g. `p->y`). Odd `disp` isn't a
                // clean `int` index — bail.
                LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::DerefDisp(r, disp) }
                    if is_reg_var(r) && disp % 2 == 0 =>
                {
                    flush_call(&mut acc, out);
                    let v = Var::Reg(r);
                    self.note(v);
                    self.note_ptr(v);
                    acc = Some(Self::deref_at(Expr::Var(v), disp / 2));
                }

                // `mov al, [si+disp]` — deref a reg-var `char *` at a constant
                // offset (`char` stride 1).
                LoOp::Load { dst: Place::Byte(ByteReg::Al), src: Place::DerefDisp(r, disp) }
                    if is_reg_var(r) =>
                {
                    // `*(p+disp) op= K` — char compound on a struct field / fixed
                    // element through a reg-var pointer at a non-zero offset
                    // (`s->c op= K`): `mov al,[si+d]; add al,K; mov [si+d],al`.
                    // Recover the load-op-store-SAME-lvalue pattern as a Compound
                    // (the Assign `*(p+d) = *(p+d) op K` double-derefs). The
                    // struct type is lost, so it emits `p[d] op= K` over a `char
                    // *` — recompiled via the reg-resident char-pointer compound
                    // path in emit_array_compound_assign. ARITH only (Add/Sub):
                    // BCC folds char `-=` to `add al,(-K)`, so a recovered Add
                    // covers both; bitwise char compounds go mem-direct and aren't
                    // emitted by that bcc path yet, so leave them incomplete.
                    if acc.is_none()
                        && let Some(LoOp::Bin {
                            dst: Place::Byte(ByteReg::Al),
                            op: op @ (BinOp::Add | BinOp::Sub),
                            lhs: Place::Byte(ByteReg::Al),
                            rhs: Place::Imm(v),
                        }) = self.insns.get(i + 1).map(|n| n.op.clone())
                        && matches!(
                            self.insns.get(i + 2).map(|n| &n.op),
                            Some(LoOp::Store { dst: Place::DerefDisp(r2, d2), src: Place::Byte(_) })
                                if *r2 == r && *d2 == disp
                        )
                    {
                        let p = Var::Reg(r);
                        self.note(p);
                        self.note_char_ptr(p);
                        out.push(Stmt::Compound(
                            LValue::Deref(Box::new(Self::offset_ptr(Expr::Var(p), disp))),
                            op,
                            Expr::Const(v),
                        ));
                        skip = 2;
                    } else {
                        flush_call(&mut acc, out);
                        let v = Var::Reg(r);
                        self.note(v);
                        self.note_char_ptr(v);
                        acc = Some(Self::deref_at(Expr::Var(v), disp));
                    }
                }

                // `*p++` / `*p--` — `mov bx,si` snapshots the pointer, then `inc/dec
                // si` (×stride) advances it, and `mov ax,[bx]` derefs the OLD value.
                // Recover the whole idiom as one post-increment deref.
                LoOp::Load { dst: Place::Reg(Reg::Bx), src: Place::Reg(r) }
                    if is_reg_var(r) && self.post_inc_deref(i, r).is_some() =>
                {
                    flush_call(&mut acc, out);
                    let (deref_idx, dec) = self.post_inc_deref(i, r).expect("checked");
                    let v = Var::Reg(r);
                    self.note(v);
                    self.note_ptr(v);
                    acc = Some(Expr::PostIncDeref(v, dec));
                    skip = deref_idx - i; // consume the incs and the deref
                }

                // `mov bx, …` loads a pointer for a following `[bx]` dereference.
                LoOp::Load { dst: Place::Reg(Reg::Bx), src } => {
                    flush_call(&mut acc, out);
                    bx = self.operand(src);
                    if bx.is_none() {
                        self.cant();
                    }
                }

                // `shl bx, s` — scale a *local array* index to a byte offset. The
                // index sits in bx (a local array's base comes via `lea` into ax,
                // so BCC scales the index in the other register); the following
                // `add bx,ax` combines them. (A pointer index scales in ax, the
                // existing foldable-shift path.)
                LoOp::Bin {
                    dst: Place::Reg(Reg::Bx),
                    op: BinOp::Shl,
                    lhs: Place::Reg(Reg::Bx),
                    rhs: Place::Imm(s),
                } => match bx.take() {
                    Some(e) => {
                        bx = Some(Expr::Binary(BinOp::Shl, Box::new(e), Box::new(Expr::Const(s))));
                    }
                    None => self.cant(),
                },

                // `add bx, ax` — index by a *variable*. Two shapes, told apart by
                // the base's **provenance**:
                //   • pointer (`p[i]`): bx holds the pointer `p` (a `mov bx,[p]`
                //     loaded value), ax the scaled index → `p + i`.
                //   • local array (`a[i]`): ax holds `&a` (a `lea ax,[bp-N]`), bx
                //     the scaled index → `&a + i`, which the deref reads as `a[i]`.
                // Either way strip the stride shift to recover the C-level index.
                LoOp::Bin {
                    dst: Place::Reg(Reg::Bx),
                    op: BinOp::Add,
                    lhs: Place::Reg(Reg::Bx),
                    rhs: Place::Reg(Reg::Ax),
                } => match (bx.take(), acc.take()) {
                    // Array: the `lea` base is in ax, the index in bx.
                    (Some(index), Some(base @ Expr::AddrOf(_))) => {
                        let (idx, shifts) = strip_scale(index);
                        let combined = Expr::Binary(BinOp::Add, Box::new(base), Box::new(idx));
                        // A `<<2` scale is a 4-byte stride — a `long` array. (The
                        // read also confirms it via the `dx:ax` pair, but a `long`
                        // array *store* writes only the low word, so the shift is
                        // its only type signal.)
                        if shifts >= 2 {
                            self.note_long_array_base(&combined);
                        }
                        bx = Some(combined);
                    }
                    // Pointer: the loaded pointer is in bx, the index in ax.
                    (Some(Expr::Var(p)), Some(index)) => {
                        self.note_ptr(p);
                        let (idx, _) = strip_scale(index);
                        bx = Some(Expr::Binary(BinOp::Add, Box::new(Expr::Var(p)), Box::new(idx)));
                    }
                    _ => self.cant(),
                },

                // `lea ax, [bp+disp]` — the address of a variable (`&x`).
                LoOp::Lea { dst: Place::Reg(Reg::Ax), src } => {
                    flush_call(&mut acc, out);
                    match Self::var_of(src) {
                        Some(v) => {
                            self.note(v);
                            acc = Some(Expr::AddrOf(v));
                        }
                        None => self.cant(),
                    }
                }

                // `xor dx,dx` — zero the high word of a `long` (high of a constant).
                LoOp::Bin { dst: Place::Reg(Reg::Dx), op: BinOp::Xor, lhs: Place::Reg(Reg::Dx), rhs: Place::Reg(Reg::Dx) } => {
                    dx = Some(DxState::Const(0));
                }

                // `mov dx, imm` — the (non-zero) high word of a `long` constant.
                LoOp::Load { dst: Place::Reg(Reg::Dx), src: Place::Imm(h) } => {
                    dx = Some(DxState::Const(h));
                }

                // Reversed `long` load: `mov ax,[hi]; mov dx,[lo]` (ax = high
                // word), the layout BCC uses when a `long` result is stored to a
                // local rather than returned. The preceding `mov ax,[lo+2]` set the
                // accumulator to the high slot as an `int`; redo it as the `long`
                // variable at `lo`, dropping that stray high-slot `int`.
                LoOp::Load { dst: Place::Reg(Reg::Dx), src: Place::Local(lo) }
                    if matches!(
                        self.insns.get(i.wrapping_sub(1)).map(|n| &n.op),
                        Some(LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::Local(hi) })
                            if *hi == lo + 2
                    ) =>
                {
                    match Self::var_of(Place::Local(lo)) {
                        Some(v) => {
                            if let Some(high) = Self::var_of(Place::Local(lo + 2)) {
                                self.vars.retain(|x| *x != high);
                            }
                            self.note(v);
                            self.note_long(v);
                            acc = Some(Expr::Var(v));
                            acc_long = true;
                        }
                        None => self.cant(),
                    }
                }

                // `mov dx, [slot]` — load the high word of a `long` variable.
                LoOp::Load { dst: Place::Reg(Reg::Dx), src: Place::Local(hi) } => {
                    dx = Some(DxState::High(hi));
                }

                // `mov ax, dx` right after an `idiv` — take the remainder (`%`).
                LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::Reg(Reg::Dx) }
                    if dx_rem.is_some() =>
                {
                    let (l, r) = dx_rem.take().expect("checked");
                    acc = Some(Expr::Binary(BinOp::Mod, Box::new(l), Box::new(r)));
                }

                // `mov ax, …` starts a fresh accumulator value, discarding any
                // call result the previous statement left unused. With `dx` set
                // up, the value is a `long` (its high word in `dx`).
                LoOp::Load { dst: Place::Reg(Reg::Ax), src } => {
                    flush_call(&mut acc, out);
                    acc_long = false;
                    match (dx.take(), src) {
                        // `mov dx,h; mov ax,lo` → the `long` constant (h<<16)|lo.
                        (Some(DxState::Const(h)), Place::Imm(lo)) => {
                            acc = Some(Expr::Const((h << 16) | (lo & 0xFFFF)));
                            acc_long = true;
                        }
                        // `mov dx,[lo+2]; mov ax,[lo]` → the `long` variable at lo.
                        (Some(DxState::High(hi)), Place::Local(lo)) if hi == lo + 2 => {
                            if let Some(v) = Self::var_of(Place::Local(lo)) {
                                self.note(v);
                                self.note_long(v);
                                acc = Some(Expr::Var(v));
                                acc_long = true;
                            } else {
                                self.cant();
                                acc = None;
                            }
                        }
                        // `mov ax, imm; mov [ptr-slot], ax` — the immediate is the
                        // address of a global (`p = &g` / `p = a` for a global
                        // array), not a literal: a stack-local pointer assigned a
                        // data-segment offset. A 3-byte `mov` of an even, non-
                        // negative value into a slot that is later loaded into `bx`
                        // and dereferenced is the address (a null pointer is `xor` /
                        // a direct immediate store). Mirrors the reg-var `mov si,imm`
                        // arm; without it the address reads as `0` and recompiles to
                        // `xor` (a byte short).
                        (_, Place::Imm(v))
                            if v >= 0
                                && v % 2 == 0
                                && matches!(
                                    self.insns.get(i + 1).map(|n| &n.op),
                                    Some(LoOp::Store {
                                        dst: Place::Local(slot),
                                        src: Place::Reg(Reg::Ax),
                                    }) if self.slot_is_dereferenced(*slot)
                                ) =>
                        {
                            let g = Var::Global(u16::try_from(v).unwrap_or(0));
                            self.note(g);
                            acc = Some(Expr::AddrOf(g));
                        }
                        (_, src) => {
                            acc = self.operand(src);
                            if acc.is_none() {
                                self.cant();
                            }
                        }
                    }
                }

                // `mov al, …` loads a `char` into the accumulator (a following
                // `cbw` promotes it to `int`, handled above as a no-op). The
                // source is a `char` variable or a byte immediate (`mov al,5`).
                LoOp::Load { dst: Place::Byte(ByteReg::Al), src } => {
                    flush_call(&mut acc, out);
                    acc = match src {
                        Place::Imm(v) => Some(Expr::Const(v)),
                        // A byte load of a word-accessed slot reads the low byte of
                        // an `int` — a narrowing `(char)x`. Keep the variable `int`
                        // (don't char-mark it) and record the cast, which is what
                        // makes a `char` store re-emit the byte load (a plain
                        // `c = x` would re-evaluate `x` at word width).
                        other if self.is_word_place(other) => {
                            self.operand(other).map(|e| Expr::Cast(Type::Char, Box::new(e)))
                        }
                        other => self.char_operand(other),
                    };
                    if acc.is_none() {
                        self.cant();
                    }
                }

                // `mov ah, 0` — zero-extend `al`→`ax`: the `char` in the
                // accumulator is `unsigned` (a signed `char` would use `cbw`).
                // A no-op for the value.
                LoOp::Load { dst: Place::Byte(ByteReg::Ah), src: Place::Imm(0) } => {
                    if let Some(e) = acc.clone() {
                        self.mark_unsigned(&e);
                    }
                }

                // `mov cl, <y>` — load a variable shift count. `cl` is the 8086
                // shift register; the following `shl/shr ax,cl` consumes it. (This
                // precedes the char-reg-var arm, which would otherwise read `cl`
                // as a `char` variable.)
                LoOp::Load { dst: Place::Byte(ByteReg::Cl), src } => {
                    cl = match src {
                        Place::Imm(v) => Some(Expr::Const(v)),
                        other => self.operand(other),
                    };
                    if cl.is_none() {
                        self.cant();
                    }
                }

                // `mov dl, …` — assign to a `char` register variable. The source
                // is the accumulator (`mov dl,al`), an immediate, or another
                // `char` variable.
                LoOp::Load { dst: Place::Byte(r), src } if is_byte_reg_var(r) => {
                    let val = match src {
                        Place::Byte(ByteReg::Al) => acc.take(),
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.char_operand(other)
                        }
                    };
                    match (self.char_dest(Place::Byte(r)), val) {
                        (Some(lv), Some(e)) => out.push(Stmt::Assign(lv, e)),
                        _ => self.cant(),
                    }
                }

                // `mov si, …` — an assignment to a register variable. The source
                // is an immediate, the accumulator (`mov si,ax` stores the
                // current expression), or another register variable.
                LoOp::Load { dst: Place::Reg(r), src } if is_reg_var(r) => {
                    let val = match src {
                        Place::Reg(Reg::Ax) => acc.take(), // consumes the accumulator
                        // A pointer reg var (it's dereferenced) loaded with an
                        // immediate is `&global` — the global at that data-segment
                        // offset — not a literal. (A null pointer is `xor`, a
                        // distinct op.) The address forces a fixup'd 3-byte `mov`.
                        Place::Imm(v)
                            if v >= 0 && v % 2 == 0 && self.reg_is_dereferenced(r) =>
                        {
                            flush_call(&mut acc, out);
                            let g = Var::Global(u16::try_from(v).unwrap_or(0));
                            self.note(g);
                            Some(Expr::AddrOf(g))
                        }
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.operand(other)
                        }
                    };
                    match (self.dest(Place::Reg(r)), val) {
                        (Some(lv), Some(e)) => out.push(Stmt::Assign(lv, e)),
                        _ => self.cant(),
                    }
                }

                // `<op> ax, [bx]` — the accumulator combined with `*p`.
                LoOp::Bin {
                    dst: Place::Reg(Reg::Ax),
                    op,
                    lhs: Place::Reg(Reg::Ax),
                    rhs: Place::Deref(Reg::Bx),
                } if is_foldable(op) => match (acc.take(), bx.clone()) {
                    (Some(l), Some(ptr)) => {
                        if let Expr::Var(v) = ptr {
                            self.note_ptr(v);
                        }
                        acc = Some(Expr::Binary(op, Box::new(l), Box::new(Expr::Deref(Box::new(ptr)))));
                    }
                    _ => self.cant(),
                },

                // `<op> ax, dx` — combine the restored left operand (in `ax`)
                // with the right operand saved in `dx`: `left <op> right`. The
                // tail of the binary-operand spill (`push ax` … `pop ax`).
                LoOp::Bin { dst: Place::Reg(Reg::Ax), op, lhs: Place::Reg(Reg::Ax), rhs: Place::Reg(Reg::Dx) }
                    if is_foldable(op) =>
                {
                    match (acc.take(), dx_temp.take()) {
                        (Some(l), Some(r)) => {
                            acc = Some(Expr::Binary(op, Box::new(l), Box::new(r)));
                        }
                        _ => self.cant(),
                    }
                }

                // The low-word half of a `long` add/sub — `add ax,lo` / `sub ax,lo`
                // (dx-high layout) or `add dx,lo` / `sub dx,lo` (the reversed
                // ax-high layout used for a store-to-local). The high-word
                // `adc`/`sbb` below completes it; defer until then.
                LoOp::Bin { dst: Place::Reg(d), op, lhs: Place::Reg(l), rhs }
                    if acc_long
                        && l == d
                        && matches!(d, Reg::Ax | Reg::Dx)
                        && matches!(op, BinOp::Add | BinOp::Sub) =>
                {
                    pending_long = Some((op, rhs));
                }

                // `adc dx,hi` / `sbb dx,hi` (dx-high) or `adc ax,hi` / `sbb ax,hi`
                // (the reversed ax-high layout) — the high-word half of a `long`
                // add/sub. The operand is a `long` variable (`[lo]`/`[lo+2]`) or a
                // constant (`(hi<<16)|lo`).
                LoOp::Bin {
                    dst: Place::Reg(d),
                    op: BinOp::Adc | BinOp::Sbb,
                    lhs: Place::Reg(l),
                    rhs: rhs_hi,
                } if l == d && matches!(d, Reg::Ax | Reg::Dx) => match pending_long.take() {
                    Some((op_lo, rhs_lo)) => match (acc.take(), self.long_operand(rhs_lo, rhs_hi)) {
                        (Some(l), Some(r)) => {
                            // BCC mis-folds `x + (negative long literal)` (it loses
                            // the high word), but compiles `x - <positive>` right.
                            // So flip a negative `long` constant to a subtraction.
                            let (op, r) = match r {
                                Expr::LongConst(v) if v < 0 && v != i32::MIN => {
                                    (flip_addsub(op_lo), Expr::LongConst(-v))
                                }
                                other => (op_lo, other),
                            };
                            acc = Some(Expr::Binary(op, Box::new(l), Box::new(r)));
                        }
                        _ => self.cant(),
                    },
                    None => self.cant(),
                },

                LoOp::Bin { dst: Place::Reg(Reg::Ax), op, lhs: Place::Reg(Reg::Ax), rhs }
                    if is_foldable(op) =>
                {
                    // A variable shift count arrives in `cl` (`shl ax,cl`); every
                    // other operand reads directly.
                    let rhs_val = if rhs == Place::Byte(ByteReg::Cl) {
                        cl.take()
                    } else {
                        self.operand(rhs)
                    };
                    match (acc.take(), rhs_val) {
                        (Some(l), Some(r)) => {
                            // A logical right shift (`shr`, not `sar`) means the
                            // shifted value is `unsigned`.
                            if op == BinOp::Shr {
                                self.mark_unsigned(&l);
                            }
                            // BCC unrolls a constant shift into shift-by-1s, so
                            // collapse `(x <shift> a) <shift> b` back to
                            // `x <shift> (a+b)` — re-emitting nested shifts would
                            // make the intermediate signed (`sar` not `shr`).
                            acc = Some(combine_shift(op, l, r));
                        }
                        _ => self.cant(),
                    }
                }

                // `<op> al, K` — extend the byte accumulator with a constant, but
                // ONLY when it holds a plain `char` GLOBAL or a `char` REGISTER
                // variable just loaded. BCC's AL-through char compound `c op= K`
                // is `mov al,<c>; add al,K; mov <c>,al`; the matching store
                // recovers `c = c op K`, which recompiles to the identical
                // AL-through for a global (`mov [g],al`) or a char reg-var (`mov
                // dl,al`) — both verified byte-identical to `c op= K`. For a char
                // LOCAL stack slot / DEREF / array element / struct-local field,
                // `x = x op K` does NOT recompile like `x op= K` (extra reload /
                // different frame), so those stay incomplete (mismatch fixtures
                // 707/710/711/719/1384) until a load-op-store-same-lvalue
                // *compound* recognizer lands. Constant rhs only; a byte-register
                // rhs is the `AluByteReg` (char op char) path.
                LoOp::Bin {
                    dst: Place::Byte(ByteReg::Al),
                    op,
                    lhs: Place::Byte(ByteReg::Al),
                    rhs: Place::Imm(v),
                } if is_foldable(op)
                    && matches!(acc, Some(Expr::Var(Var::Global(_) | Var::ByteReg(_)))) =>
                {
                    let l = acc.take().unwrap();
                    acc = Some(Expr::Binary(op, Box::new(l), Box::new(Expr::Const(v))));
                }

                // `imul <operand>` — signed multiply; the `int` result is the low
                // word (`ax`). The operand is a memory operand or a constant the
                // multiplier loaded into `dx` (`mov dx,K; imul dx`).
                LoOp::Bin { dst: Place::DxAx, op: BinOp::Imul, lhs: Place::Reg(Reg::Ax), rhs } => {
                    let r = match rhs {
                        // `dx` holds either a constant multiplier (`mov dx,K; imul
                        // dx`) or the spilled right operand (`mov dx,ax` from a
                        // two-register multiply, e.g. `char a * char b`).
                        Place::Reg(Reg::Dx) => match dx.take() {
                            Some(DxState::Const(v)) => Some(Expr::Const(v)),
                            _ => dx_temp.take(),
                        },
                        other => self.operand(other),
                    };
                    match (acc.take(), r) {
                        (Some(l), Some(r)) => {
                            acc = Some(Expr::Binary(BinOp::Imul, Box::new(l), Box::new(r)));
                        }
                        _ => self.cant(),
                    }
                }

                // `idiv <operand>` — signed divide. The quotient is the low word
                // (`ax`), the remainder the high word (`dx`); a following
                // `mov ax,dx` recovers `%`. The preceding `cwd` set up `dx:ax`
                // from the accumulator (the dividend).
                LoOp::Bin { dst: Place::DxAx, op: BinOp::Idiv, lhs: Place::DxAx, rhs } => {
                    // The divisor is a memory operand, or a constant the compiler
                    // loaded into bx (`mov bx,2; idiv bx` for `a / 2`).
                    let divisor = match rhs {
                        Place::Reg(Reg::Bx) => bx.clone(),
                        other => self.operand(other),
                    };
                    match (acc.take(), divisor) {
                        (Some(l), Some(r)) => {
                            dx_rem = Some((l.clone(), r.clone()));
                            acc = Some(Expr::Binary(BinOp::Idiv, Box::new(l), Box::new(r)));
                        }
                        _ => self.cant(),
                    }
                }

                // `xor si,si` — the zero idiom on a register variable (`x = 0`).
                LoOp::Bin { dst: Place::Reg(r), op: BinOp::Xor, lhs: Place::Reg(a), rhs: Place::Reg(b) }
                    if is_reg_var(r) && a == r && b == r =>
                {
                    match self.dest(Place::Reg(r)) {
                        Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(0))),
                        None => self.cant(),
                    }
                }

                // `neg ax`/`neg al` — arithmetic negation `-x`, unless it opens
                // the `!x` idiom `neg ax; sbb ax,ax; inc ax` (which leaves 0/1 for
                // logical not). The `sbb`/`inc` tail is then consumed.
                LoOp::Un { dst: Place::Reg(Reg::Ax), op: UnOp::Neg, operand: Place::Reg(Reg::Ax) }
                | LoOp::Un {
                    dst: Place::Byte(ByteReg::Al),
                    op: UnOp::Neg,
                    operand: Place::Byte(ByteReg::Al),
                } => {
                    let is_lognot = matches!(
                        self.insns.get(i + 1).map(|n| &n.op),
                        Some(LoOp::Bin {
                            dst: Place::Reg(Reg::Ax),
                            op: BinOp::Sbb,
                            lhs: Place::Reg(Reg::Ax),
                            rhs: Place::Reg(Reg::Ax),
                        })
                    ) && matches!(
                        self.insns.get(i + 2).map(|n| &n.op),
                        Some(LoOp::Un {
                            dst: Place::Reg(Reg::Ax),
                            op: UnOp::Inc,
                            operand: Place::Reg(Reg::Ax),
                        })
                    );
                    match acc.take() {
                        Some(e) if is_lognot => {
                            acc = Some(Expr::Not(Box::new(e)));
                            skip = 2;
                        }
                        Some(e) => acc = Some(Expr::Unary(UnaryOp::Neg, Box::new(e))),
                        None => self.cant(),
                    }
                }

                // `not ax`/`not al` — bitwise complement `~x`.
                LoOp::Un { dst: Place::Reg(Reg::Ax), op: UnOp::Not, operand: Place::Reg(Reg::Ax) }
                | LoOp::Un {
                    dst: Place::Byte(ByteReg::Al),
                    op: UnOp::Not,
                    operand: Place::Byte(ByteReg::Al),
                } => match acc.take() {
                    Some(e) => acc = Some(Expr::Unary(UnaryOp::BitNot, Box::new(e))),
                    None => self.cant(),
                },

                // `inc`/`dec ax` (or byte `inc al`) extends the accumulator by
                // ±1 (`x = x + 1` / char `c = c + 1` kept byte-wide).
                LoOp::Un { dst: Place::Reg(Reg::Ax), op, operand: Place::Reg(Reg::Ax) }
                | LoOp::Un { dst: Place::Byte(ByteReg::Al), op, operand: Place::Byte(ByteReg::Al) }
                    if matches!(op, UnOp::Inc | UnOp::Dec) =>
                {
                    let step = if matches!(op, UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match acc.take() {
                        Some(e) => acc = Some(Expr::Binary(step, Box::new(e), Box::new(Expr::Const(1)))),
                        None => self.cant(),
                    }
                }

                // `inc`/`dec dl` — an in-place `c++`/`c--` on a char register
                // variable. BCC codes this in one instruction when the result is
                // used right after; the discarded form is a load-op-store (`mov
                // al,dl; inc al; mov dl,al`) that folds to a plain `Assign` via the
                // accumulator. So the single `inc dl` recovers as a `Compound`.
                LoOp::Un { dst: Place::Byte(r), op, operand: Place::Byte(o) }
                    if is_byte_reg_var(r) && o == r && matches!(op, UnOp::Inc | UnOp::Dec) =>
                {
                    let step = if matches!(op, UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match self.char_dest(Place::Byte(r)) {
                        Some(lv) => out.push(Stmt::Compound(lv, step, Expr::Const(1))),
                        None => self.cant(),
                    }
                }

                // In-place byte compound on a `char` register variable: `add
                // dl, al` (rhs computed into `al`), `or dl, 7` (immediate), or
                // `add dl, bl` (another `char` variable). The byte analog of the
                // word in-place compound — `a op= b` for chars, applied in place.
                // The rhs reached through `al` can be an `int`'s low byte
                // (`c |= n`, n int): that's a valid `char op= int`, kept correct
                // by typing the rhs from its own (word) accesses, not forcing it
                // `char` — see the word-slot pass in `recover_window`.
                LoOp::Bin { dst: Place::Byte(r), op, lhs: Place::Byte(l), rhs }
                    if r == l && is_byte_reg_var(r) && Self::is_compound_op(op) =>
                {
                    let rhs_expr = match rhs {
                        // A simple value in `al` — a variable (`c op= b`), a
                        // constant, or a narrowed `int` (`c op= n`, `(char)n`). A
                        // *complex* `al` (e.g. `a*b` applied through a `dl` temp,
                        // `mov dl,bl; add dl,al; mov bl,dl`) isn't a direct compound
                        // on this register — decline rather than mis-attribute it.
                        Place::Byte(ByteReg::Al) => match acc.take() {
                            v @ Some(Expr::Var(_) | Expr::Const(_)) => v,
                            // The compound's byte op narrows implicitly, so drop a
                            // `(char)` the load attached — `c |= n`, not
                            // `c |= (char)n`.
                            Some(Expr::Cast(_, inner)) if matches!(*inner, Expr::Var(_)) => {
                                Some(*inner)
                            }
                            _ => None,
                        },
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.operand(other)
                        }
                    };
                    match (self.char_dest(Place::Byte(r)), rhs_expr) {
                        (Some(lv), Some(e)) => {
                            // The rhs may be an `int` whose low byte was read into
                            // `al` (`c |= n`) — keep it `int`, not `char`.
                            self.untype_word_chars(&e);
                            out.push(Stmt::Compound(lv, op, e));
                        }
                        _ => self.cant(),
                    }
                }

                // In-place `inc`/`dec` on a word variable — `inc si` (register
                // variable), `inc word [g]` (global), `inc word [bp-4]` (local).
                // A single-instruction `++`/`--`, distinct from the load-op-store
                // `x = x ± 1` (which routes through the accumulator above). The
                // `char` byte form is handled just above; the accumulator `inc
                // ax`/`inc al` extends the expression and is handled earlier.
                LoOp::Un { dst, op, operand }
                    if dst == operand
                        && matches!(op, UnOp::Inc | UnOp::Dec)
                        && !matches!(dst, Place::Byte(_))
                        && Self::var_of(dst).is_some() =>
                {
                    let step = if matches!(op, UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match self.dest(dst) {
                        Some(lv) => out.push(Stmt::Compound(lv, step, Expr::Const(1))),
                        None => self.cant(),
                    }
                }

                // In-place byte `inc`/`dec` on a `char` GLOBAL lvalue — `inc
                // byte [g]` (char global / struct field). The byte width marks
                // the lvalue `char`, unlike the word `Un` arm above. A single-
                // instruction `++`/`--`. Stage 4 mem-RMW (`0xFE 06/0e`).
                //
                // Globals are safe: the data segment is modelled as scalars at
                // their exact recovered offsets, so `gv@k++` recompiles to the
                // same `inc byte [_+k]`. The byte-LOCAL form (`0xFE [bp+disp]`)
                // is intentionally left opaque (not even recognized as an idiom)
                // — a `char` array element (`a[K]++`) is byte-indistinguishable
                // from a scalar char local but reg-allocates / lays out
                // differently, so lifting it would expose the array-vs-scalars
                // mismatch (fixture 721). The `Place::Global` bound here keeps
                // the arm honest if a non-global `UnByte` ever appears.
                //
                // Only the DISCARDED form (`g++;` as a statement), guarded by an
                // empty accumulator. A value-using post/pre-inc (`return g++`,
                // `f(c++)`) loads the operand into `al` first, leaving `acc`
                // live; recovering it as a bare `g++` statement would drop the
                // old-value use and mis-recompile (fixtures 731/733/972).
                LoOp::UnByte { dst: dst @ Place::Global(_), op, operand }
                    if dst == operand
                        && matches!(op, UnOp::Inc | UnOp::Dec)
                        && acc.is_none() =>
                {
                    let step = if matches!(op, UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match self.char_dest(dst) {
                        Some(lv) => out.push(Stmt::Compound(lv, step, Expr::Const(1))),
                        None => self.cant(),
                    }
                }

                // In-place compound through a pointer — `*p op= Y` (`add [bx],3`,
                // `or [si],dx`). The destination is a deref: `[bx]` reads whatever
                // the pointer-tracking holds (a loaded pointer, or a `p+i`/`a[i]`
                // address); `[si]`/`[di]` is a register-variable pointer. Distinct
                // from the load-op-store `*p = *p op Y`, so it recovers as `*p op= Y`.
                LoOp::Bin { dst: Place::Deref(dreg), op, lhs, rhs }
                    if Place::Deref(dreg) == lhs && Self::is_compound_op(op) =>
                {
                    let ptr = match dreg {
                        Reg::Bx => bx.clone(),
                        r => {
                            // `[si]`/`[di]` — the register variable is the pointer.
                            let v = Var::Reg(r);
                            self.note(v);
                            self.note_ptr(v);
                            Some(Expr::Var(v))
                        }
                    };
                    let rhs_expr = match rhs {
                        Place::Reg(Reg::Ax) => match acc.take() {
                            Some(e) if is_simple_compound_rhs(&e) => Some(e),
                            _ => None,
                        },
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.operand(other)
                        }
                    };
                    match (ptr, rhs_expr) {
                        // A `long` array element only writes its low word — not a
                        // clean `long` compound — so decline (the long stage's job).
                        (Some(p), _) if self.is_long_array_elem(&p) => self.cant(),
                        (Some(p), Some(e)) => {
                            if let Expr::Var(v) = &p {
                                self.note_ptr(*v);
                            }
                            out.push(Stmt::Compound(LValue::Deref(Box::new(p)), op, e));
                        }
                        _ => self.cant(),
                    }
                }

                // `*(p + K) op= Y` — an in-place compound on a struct field /
                // fixed-index element through a pointer at a constant offset
                // (`s->y op= K`, `add [si+disp],imm`). Int stride: K = disp/2.
                LoOp::Bin { dst: Place::DerefDisp(dreg, disp), op, lhs, rhs }
                    if Place::DerefDisp(dreg, disp) == lhs
                        && Self::is_compound_op(op)
                        && disp % 2 == 0
                        // A `long` element compound writes two words — this disp then
                        // disp+2, same op (`and [bx+4],lo; and [bx+6],hi`). That isn't
                        // an int field; decline (the long stage's job).
                        && !matches!(
                            self.insns.get(i + 1).map(|n| &n.op),
                            Some(LoOp::Bin { dst: Place::DerefDisp(r2, d2), op: o2, .. })
                                if *r2 == dreg && *d2 == disp + 2 && *o2 == op
                        ) =>
                {
                    let ptr = match dreg {
                        Reg::Bx => bx.clone(),
                        r if is_reg_var(r) => {
                            let v = Var::Reg(r);
                            self.note(v);
                            self.note_ptr(v);
                            Some(Expr::Var(v))
                        }
                        _ => None,
                    };
                    let rhs_expr = match rhs {
                        Place::Reg(Reg::Ax) => match acc.take() {
                            Some(e) if is_simple_compound_rhs(&e) => Some(e),
                            _ => None,
                        },
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.operand(other)
                        }
                    };
                    match (ptr, rhs_expr) {
                        (Some(p), Some(e)) => {
                            if let Expr::Var(v) = &p {
                                self.note_ptr(*v);
                            }
                            let place =
                                LValue::Deref(Box::new(Self::offset_ptr(p, disp / 2)));
                            out.push(Stmt::Compound(place, op, e));
                        }
                        _ => self.cant(),
                    }
                }

                // In-place compound modification — `op X, Y` where the
                // destination is also the left operand and a word variable
                // (`add si,5`, `add di,si`, `add [g],3`, `sub [bp-4],2`,
                // `and si,7`). BCC codes this in one instruction, distinct from the
                // load-op-store `X = X op Y`, so it recovers as `X op= Y`.
                //
                // `Y` is usually a register/local/immediate operand, but for a
                // *complex* right-hand side BCC computes it into `ax` first and
                // codes `op X, ax` — so an `ax` source takes the accumulated
                // expression (`x += a * b`), not a bare register.
                LoOp::Bin { dst, op, lhs, rhs }
                    if dst == lhs
                        && Self::is_compound_op(op)
                        && !matches!(dst, Place::Byte(_))
                        && Self::var_of(dst).is_some() =>
                {
                    let rhs_expr = if matches!(rhs, Place::Reg(Reg::Ax)) {
                        // A complex RHS is computed into `ax` first (`x += a * b`);
                        // take that accumulated value — but only plain arithmetic.
                        // A deref or call RHS exposes unrelated gaps when it lets the
                        // function complete (see `is_simple_compound_rhs`), so leave
                        // those declined.
                        match acc.take() {
                            Some(e) if is_simple_compound_rhs(&e) => Some(e),
                            _ => None,
                        }
                    } else {
                        self.operand(rhs)
                    };
                    match (self.dest(dst), rhs_expr) {
                        (Some(lv), Some(e)) => out.push(Stmt::Compound(lv, op, e)),
                        _ => self.cant(),
                    }
                }

                // `mov [bx], al` — store a `char` through a `char *` (`*p = v`).
                // The byte in `al` is the accumulator (a `char` loaded just
                // before); the pointer in bx is a `char *`.
                LoOp::Store { dst: Place::Deref(Reg::Bx), src: Place::Byte(ByteReg::Al) } => {
                    match (bx.clone(), acc.take()) {
                        (Some(ptr), Some(e)) => {
                            if let Expr::Var(v) = ptr {
                                self.note_char_ptr(v);
                            }
                            self.note_char_array_base(&ptr);
                            out.push(Stmt::Assign(LValue::Deref(Box::new(ptr)), e));
                        }
                        _ => self.cant(),
                    }
                }

                // `mov [bx], ax` / `mov [bx], imm` — store through a pointer
                // (`*p = v` / `*p = const`).
                LoOp::Store { dst: Place::Deref(Reg::Bx), src } => {
                    let value = match src {
                        Place::Reg(Reg::Ax) => acc.take(),
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.operand(other)
                        }
                    };
                    match (bx.clone(), value) {
                        // A `long` array element store writes only the low word (a
                        // BCC quirk) — not a clean `long` assignment — so decline.
                        (Some(ptr), _) if self.is_long_array_elem(&ptr) => {
                            self.cant();
                        }
                        (Some(ptr), Some(e)) => {
                            if let Expr::Var(v) = ptr {
                                self.note_ptr(v);
                            }
                            out.push(Stmt::Assign(LValue::Deref(Box::new(ptr)), e));
                        }
                        _ => self.cant(),
                    }
                }

                // `mov [si], al` / `mov [di], al` — store a `char` through a
                // reg-var `char *` pointer (`*p = v`).
                LoOp::Store { dst: Place::Deref(r), src: Place::Byte(ByteReg::Al) }
                    if is_reg_var(r) =>
                {
                    let v = Var::Reg(r);
                    match acc.take() {
                        Some(e) => {
                            self.note(v);
                            self.note_char_ptr(v);
                            out.push(Stmt::Assign(LValue::Deref(Box::new(Expr::Var(v))), e));
                        }
                        None => self.cant(),
                    }
                }

                // `mov [si], ax` / `mov [si], imm` — store through a reg-var
                // pointer (`*p = v` / `*p = const`).
                LoOp::Store { dst: Place::Deref(r), src } if is_reg_var(r) => {
                    let value = match src {
                        Place::Reg(Reg::Ax) => acc.take(),
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.operand(other)
                        }
                    };
                    match value {
                        Some(e) => {
                            let v = Var::Reg(r);
                            self.note(v);
                            self.note_ptr(v);
                            out.push(Stmt::Assign(LValue::Deref(Box::new(Expr::Var(v))), e));
                        }
                        None => self.cant(),
                    }
                }

                // `mov [bx+disp],ax` / `mov word ptr [bx+disp],imm` — store a word
                // through an `int *` at a constant offset: `*(p + K) = value`,
                // K = disp/2 (the `int` stride). An odd displacement isn't a clean
                // `int` index — bail.
                LoOp::Store { dst: Place::DerefDisp(Reg::Bx, disp), src } => {
                    let value = match src {
                        Place::Reg(Reg::Ax) => acc.take(),
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.operand(other)
                        }
                    };
                    match (bx.clone(), value) {
                        (Some(ptr), Some(e)) if disp % 2 == 0 => {
                            if let Expr::Var(v) = ptr {
                                self.note_ptr(v);
                            }
                            let place = LValue::Deref(Box::new(Self::offset_ptr(ptr, disp / 2)));
                            out.push(Stmt::Assign(place, e));
                        }
                        _ => self.cant(),
                    }
                }

                // `mov [si+disp],ax` / `mov [si+disp],imm` — store through a reg-var
                // `int *` at a constant offset (`*(p + K) = v`, K = disp/2 — an int
                // field write `p->y = v`).
                LoOp::Store { dst: Place::DerefDisp(r, disp), src }
                    if is_reg_var(r) && disp % 2 == 0 =>
                {
                    let value = match src {
                        Place::Reg(Reg::Ax) => acc.take(),
                        Place::Imm(v) => {
                            flush_call(&mut acc, out);
                            Some(Expr::Const(v))
                        }
                        other => {
                            flush_call(&mut acc, out);
                            self.operand(other)
                        }
                    };
                    match value {
                        Some(e) => {
                            let v = Var::Reg(r);
                            self.note(v);
                            self.note_ptr(v);
                            let place =
                                LValue::Deref(Box::new(Self::offset_ptr(Expr::Var(v), disp / 2)));
                            out.push(Stmt::Assign(place, e));
                        }
                        None => self.cant(),
                    }
                }

                // A `long` local store from `dx:ax`: the high word then the low to
                // a slot pair (`mov [hi],<reg>; mov [lo],<reg>`, the two registers
                // ax/dx in some order — a widened `int` keeps the high in `dx`, a
                // `long` add in `ax`). Folds to one `long` assignment of the long
                // accumulator, so the high slot never becomes a separate `int`.
                LoOp::Store { dst: Place::Local(s1), src: Place::Reg(r1) }
                    if acc_long && self.paired_long_store_low(i, s1, r1).is_some() =>
                {
                    let lo = s1 - 2;
                    match (Self::var_of(Place::Local(lo)), acc.take()) {
                        (Some(v), Some(e)) => {
                            self.note(v);
                            self.note_long(v);
                            out.push(Stmt::Assign(LValue::Var(v), e));
                            skip = 1;
                        }
                        _ => self.cant(),
                    }
                }

                LoOp::Store { dst, src: Place::Reg(Reg::Ax) } => {
                    match (self.dest(dst), acc.take()) {
                        (Some(lv), Some(e)) => out.push(Stmt::Assign(lv, e)),
                        _ => self.cant(),
                    }
                }

                // `mov [dst], al` — store the accumulator to a `char`.
                LoOp::Store { dst, src: Place::Byte(_) } => {
                    match (self.char_dest(dst), acc.take()) {
                        (Some(lv), Some(e)) => out.push(Stmt::Assign(lv, e)),
                        _ => self.cant(),
                    }
                }

                // A `long` local constant assignment is a store *pair*: the high
                // word first (`mov [hi],imm_hi`), then the low (`mov [lo],imm_lo`,
                // `lo == hi-2`). Fold both into one `long` assignment so the high
                // slot never becomes a separate `int` variable (which the
                // double-count guard in `recover` would reject). A lone immediate
                // store (no matching low half) is a plain `int`/`char` store.
                LoOp::Store { dst: Place::Local(hi), src: Place::Imm(imm_hi) } => {
                    flush_call(&mut acc, out);
                    let low_half = match self.insns.get(i + 1).map(|n| &n.op) {
                        Some(LoOp::Store { dst: Place::Local(lo), src: Place::Imm(imm_lo) })
                            if *lo == hi - 2 && self.long_slots.contains(lo) =>
                        {
                            Some(*imm_lo)
                        }
                        _ => None,
                    };
                    match low_half.and_then(|imm_lo| {
                        Self::var_of(Place::Local(hi - 2)).map(|var| (var, imm_lo))
                    }) {
                        Some((var, imm_lo)) => {
                            self.note(var);
                            self.note_long(var);
                            let value = (imm_hi << 16) | (imm_lo & 0xFFFF);
                            out.push(Stmt::Assign(LValue::Var(var), Expr::LongConst(value)));
                            skip = 1;
                        }
                        None => match self.dest(Place::Local(hi)) {
                            Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(imm_hi))),
                            None => self.cant(),
                        },
                    }
                }

                LoOp::Store { dst, src: Place::Imm(v) } => {
                    flush_call(&mut acc, out);
                    match self.dest(dst) {
                        Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(v))),
                        None => self.cant(),
                    }
                }

                // `mov byte ptr [bx], imm` — a `char` immediate stored through a
                // `char *` (`*p = const`).
                // `mov byte [p+disp], imm` — `cp[K] = const` through a `char *` at a
                // constant offset (`*(cp + K) = const`). `[bx]` reads the tracked
                // pointer; `[si]`/`[di]` is a reg-var `char *`. Char stride is 1, so
                // the offset is the byte disp directly.
                LoOp::StoreImmByte { dst: Place::DerefDisp(r, disp), imm } => {
                    flush_call(&mut acc, out);
                    let ptr = match r {
                        Reg::Bx => bx.clone(),
                        // A reg-var `char *` at a constant offset is often the decay
                        // of a local `char` array (`p = a; p[1] = …`), which
                        // detect_local_array can't yet model alongside the pointer
                        // (it mis-recovers as scalars) — leave those to the array
                        // stage rather than emit wrong bytes.
                        _ => None,
                    };
                    match ptr {
                        Some(p) => {
                            if let Expr::Var(v) = &p {
                                self.note_char_ptr(*v);
                            }
                            let place = LValue::Deref(Box::new(Self::offset_ptr(p, disp)));
                            out.push(Stmt::Assign(place, Expr::Const(imm)));
                        }
                        None => self.cant(),
                    }
                }
                LoOp::StoreImmByte { dst: Place::Deref(Reg::Bx), imm } => {
                    flush_call(&mut acc, out);
                    match bx.clone() {
                        Some(ptr) => {
                            if let Expr::Var(v) = ptr {
                                self.note_char_ptr(v);
                            }
                            out.push(Stmt::Assign(LValue::Deref(Box::new(ptr)), Expr::Const(imm)));
                        }
                        None => self.cant(),
                    }
                }

                // `mov byte ptr [si]/[di], imm` — a `char` immediate stored
                // through a reg-var `char *` (`*p = const`, e.g. `*p++ = K`).
                LoOp::StoreImmByte { dst: Place::Deref(r), imm } if is_reg_var(r) => {
                    flush_call(&mut acc, out);
                    let v = Var::Reg(r);
                    self.note(v);
                    self.note_char_ptr(v);
                    out.push(Stmt::Assign(
                        LValue::Deref(Box::new(Expr::Var(v))),
                        Expr::Const(imm),
                    ));
                }

                // `mov byte ptr [dst], imm` — a `char` immediate store.
                LoOp::StoreImmByte { dst, imm } => {
                    flush_call(&mut acc, out);
                    match self.char_dest(dst) {
                        Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(imm))),
                        None => self.cant(),
                    }
                }

                _ => self.cant(),
            }
        }
        // A discarded call at the run's end (a trailing `g(x);`) still happened.
        flush_call(&mut acc, out);
        if !pending_args.is_empty() {
            self.cant(); // args pushed with no call to consume them
        }
        acc
    }

    /// Recover the condition of an `if`/`while` from the test at `cmp_idx` and
    /// the branch at `branch_idx`. `negate` inverts it (for a skip branch).
    ///
    /// Two test shapes feed a branch and must be told apart because they emit
    /// differently: an explicit `cmp lhs, rhs` recovers as a relational
    /// expression, while the register-variable truthiness idiom `or r,r` (which
    /// sets flags without changing `r`) recovers as the *bare* variable (`if (x)`
    /// / `if (!x)`). Modelling the latter as `x != 0` would recompile to a `cmp`,
    /// not the original `or`.
    fn condition(
        &mut self,
        cmp_idx: usize,
        branch_idx: usize,
        negate: bool,
        acc: Option<&Expr>,
    ) -> Expr {
        let bad = |s: &mut Self| {
            s.complete = false;
            Expr::Const(0)
        };
        let LoOp::Branch { cond, .. } = self.insns[branch_idx].op else {
            return bad(self);
        };
        let Some(mut rel) = cond_to_relop(cond) else {
            return bad(self);
        };
        if negate {
            rel = rel.negate();
        }

        // A truthiness test `or r,r` — a register variable (`si`/`di`/`dl`) or
        // the accumulator holding a computed value (`mov ax,[bx]; or ax,ax` for
        // `*p`). `or r,r` sets the flags from `r` without changing it.
        let truthy: Option<Expr> = match self.insns[cmp_idx].op.clone() {
            LoOp::Bin { dst: Place::Reg(r), op: BinOp::Or, lhs: Place::Reg(a), rhs: Place::Reg(b) }
                if a == r && b == r =>
            {
                if is_reg_var(r) {
                    self.operand(Place::Reg(r))
                } else if r == Reg::Ax {
                    acc.cloned()
                } else {
                    None
                }
            }
            LoOp::Bin { dst: Place::Byte(r), op: BinOp::Or, lhs: Place::Byte(a), rhs: Place::Byte(b) }
                if is_byte_reg_var(r) && a == r && b == r =>
            {
                self.operand(Place::Byte(r))
            }
            _ => None,
        };
        if let Some(e) = truthy {
            // The branch tests `e <rel> 0`. Equality renders as the bare/negated
            // value (so a register-variable test recompiles to `or`, not `cmp`);
            // the signed relations render as `e <rel> 0`.
            return match rel {
                RelOp::Ne => e,                      // if (x)
                RelOp::Eq => Expr::Not(Box::new(e)),  // if (!x)
                // A signed/unsigned relation against 0 (`if (x <rel> 0)`).
                _ => Expr::Rel(rel, Box::new(e), Box::new(Expr::Const(0))),
            };
        }

        // A byte-width comparison (`cmp byte ptr [c],5`) marks its operands
        // `char` — without this a `char` only ever compared would declare as
        // `int` and re-emit a word `cmp`.
        if let LoOp::CmpByte { lhs, rhs } = self.insns[cmp_idx].op.clone() {
            return match (self.cmp_operand(lhs, acc), self.cmp_operand(rhs, acc)) {
                (Some(l), Some(r)) => {
                    self.mark_char(&l);
                    self.mark_char(&r);
                    if rel.is_unsigned() {
                        self.mark_unsigned(&l);
                        self.mark_unsigned(&r);
                    }
                    Expr::Rel(rel, Box::new(l), Box::new(r))
                }
                _ => bad(self),
            };
        }

        // Otherwise an explicit (word) comparison. An operand may be the
        // accumulator (`cmp ax,n` comparing two memory operands, or `cmp ax,5` on
        // a computed value like `*p`), resolved to the value the run left in `ax`.
        let LoOp::Bin { dst: Place::Flags, op: BinOp::Cmp, lhs, rhs } = self.insns[cmp_idx].op.clone()
        else {
            return bad(self);
        };
        match (self.cmp_operand(lhs, acc), self.cmp_operand(rhs, acc)) {
            (Some(l), Some(r)) => {
                // An unsigned comparison (`jb`/`ja`) means its operands are
                // `unsigned` — mark them so the compare re-emits unsigned.
                if rel.is_unsigned() {
                    self.mark_unsigned(&l);
                    self.mark_unsigned(&r);
                }
                Expr::Rel(rel, Box::new(l), Box::new(r))
            }
            _ => bad(self),
        }
    }

    /// A `cmp` operand: a direct operand via [`operand`](Self::operand), or the
    /// accumulator (`ax`/`al`) resolved to the value the test run computed.
    fn cmp_operand(&mut self, place: Place, acc: Option<&Expr>) -> Option<Expr> {
        if matches!(place, Place::Reg(Reg::Ax) | Place::Byte(ByteReg::Al)) {
            acc.cloned()
        } else if let Place::Deref(r) = place {
            // `cmp [si],n` for `*p <rel> n` — a register-variable pointer
            // dereferenced directly in the comparison.
            if is_reg_var(r) {
                let v = Var::Reg(r);
                self.note_ptr(v);
                Some(Expr::Deref(Box::new(Expr::Var(v))))
            } else {
                None
            }
        } else {
            self.operand(place)
        }
    }

    /// If the then-block ending at the branch-target index `tb` is followed by
    /// an `else`, return the index one past the else-block. The marker is an
    /// unconditional jump just before `tb` that skips past `then_target`.
    fn else_end(&self, tb: usize, then_target: usize) -> Option<usize> {
        if tb == 0 {
            return None;
        }
        let LoOp::Jump { target: e } = self.insns[tb - 1].op else {
            return None;
        };
        // A jump to the epilogue ends the then-block with a `return`, not a skip
        // over an else-block — that's a plain `if` whose body returns early.
        if e <= then_target || self.is_epilogue(e) {
            return None;
        }
        self.idx_of(e)
    }

    /// A ternary `cond ? then : else`: the then-block `[then_lo, tb-1)` (its
    /// trailing jmp to the merge is at `tb-1`) and the else-block `[tb, e_idx)`
    /// each fold to a single accumulator value with **no emitted statements** —
    /// a pure value. Returns the `Expr::Ternary`, or `None` when either arm
    /// carries a statement (a real `if`/`else`) or doesn't leave a value.
    fn detect_ternary(
        &mut self,
        then_lo: usize,
        tb: usize,
        e_idx: usize,
        cond: &Expr,
    ) -> Option<Expr> {
        let then_hi = tb.checked_sub(1)?;
        let mut buf = Vec::new();
        let then_val = self.fold_linear(then_lo, then_hi, &mut buf)?;
        if !buf.is_empty() {
            return None;
        }
        let mut buf2 = Vec::new();
        let else_val = self.fold_linear(tb, e_idx, &mut buf2)?;
        if !buf2.is_empty() {
            return None;
        }
        Some(Expr::Ternary(Box::new(cond.clone()), Box::new(then_val), Box::new(else_val)))
    }

    /// Recognize a `switch` compare-chain starting at the `cmp` index
    /// `first_cmp`: consecutive `cmp ax,Ki; je Ti` pairs ending in an
    /// unconditional `jmp` to the no-match target. Returns the `(value, body
    /// index)` cases and the index of the no-match (post-switch) block, or
    /// `None`. Needs ≥ 2 cases — a single `cmp/je` is a plain `if`.
    fn detect_switch(&self, first_cmp: usize) -> Option<(Vec<(i32, usize)>, usize)> {
        let mut cases: Vec<(i32, usize)> = Vec::new();
        let mut c = first_cmp;
        while let Some(case) = self.switch_case_at(c) {
            cases.push(case);
            c += 2;
        }
        if cases.len() < 2 {
            return None;
        }
        // The chain ends in an unconditional jump to the no-match block.
        let LoOp::Jump { target: def } = self.insns.get(c)?.op else {
            return None;
        };
        let def_idx = self.idx_of(def)?;
        Some((cases, def_idx))
    }

    /// One `cmp ax,K; je T` link of a switch compare-chain at index `c`: the case
    /// value and the body's instruction index, or `None` if it isn't that shape.
    fn switch_case_at(&self, c: usize) -> Option<(i32, usize)> {
        let value = match self.insns.get(c).map(|n| &n.op) {
            Some(LoOp::Bin {
                dst: Place::Flags,
                op: BinOp::Cmp,
                lhs: Place::Reg(Reg::Ax),
                rhs: Place::Imm(k),
            }) => *k,
            _ => return None,
        };
        let (cond, target) = match self.insns.get(c + 1).map(|n| &n.op) {
            Some(LoOp::Branch { cond, target }) => (*cond, *target),
            _ => return None,
        };
        if cond_to_relop(cond) != Some(RelOp::Eq) {
            return None;
        }
        let ti = self.idx_of(target)?;
        Some((value, ti))
    }

    /// Assemble a recovered `switch`: resolve its `default`/continuation, build
    /// the case arms (breaking to the continuation), push the `Stmt::Switch`, and
    /// return the index to resume structuring at (the post-switch code).
    fn emit_switch(
        &mut self,
        stmts: &mut Vec<Stmt>,
        scrutinee: Expr,
        cases: &[(i32, usize)],
        def_idx: usize,
        hi: usize,
    ) -> usize {
        let (default_body, cont_idx) = self.switch_default(def_idx, hi);
        let cont_off = self.insns[cont_idx].span.start;
        let arms = self.build_switch_arms(cases, def_idx, cont_off);
        stmts.push(Stmt::Switch(scrutinee, arms, default_body));
        cont_idx
    }

    /// Build the arms of a recovered `switch` from `(case value, body start
    /// index)` pairs. The case bodies span `[start, end_idx)` (each running to
    /// the next case's start, the last to `end_idx`); a body ending in a jump to
    /// `cont_off` (the post-switch continuation) ends in `break`, one ending at
    /// the epilogue is a `return`.
    fn build_switch_arms(
        &mut self,
        cases: &[(i32, usize)],
        end_idx: usize,
        cont_off: usize,
    ) -> Vec<(i32, Vec<Stmt>)> {
        let mut arms = Vec::new();
        for (j, &(value, start)) in cases.iter().enumerate() {
            let end = cases.get(j + 1).map_or(end_idx, |&(_, s)| s);
            arms.push((value, self.switch_body(start, end, cont_off)));
        }
        arms
    }

    /// Structure one `switch` case (or `default`) body in `[start, end)`,
    /// turning a trailing jump to the continuation `cont_off` into a `break`.
    fn switch_body(&mut self, start: usize, end: usize, cont_off: usize) -> Vec<Stmt> {
        let breaks = end > start
            && matches!(self.insns[end - 1].op, LoOp::Jump { target } if target == cont_off);
        let body_end = if breaks { end - 1 } else { end };
        let mut body = self.structure(start, body_end);
        if breaks {
            body.push(Stmt::Break);
        }
        body
    }

    /// Resolve a `switch`'s no-match block. If it's a `default:` that ends in a
    /// `break` (a jump to a *further* continuation, not the epilogue), return the
    /// recovered `default` body and the continuation index. Otherwise (no default,
    /// or a `default` that returns) the no-match block *is* the post-switch code:
    /// an empty body and `def_idx` itself.
    fn switch_default(&mut self, def_idx: usize, hi: usize) -> (Vec<Stmt>, usize) {
        let def_off = self.insns[def_idx].span.start;
        // The default block's terminator: the first jump in it.
        for j in def_idx..hi {
            match self.insns[j].op {
                LoOp::Jump { target }
                    if target > def_off && !self.is_epilogue(target) =>
                {
                    if let Some(cont_idx) = self.idx_of(target) {
                        // A `default:` that breaks to `target`.
                        let body = self.switch_body(def_idx, j + 1, target);
                        return (body, cont_idx);
                    }
                    break;
                }
                // A return-jump (to the epilogue) or the epilogue itself: the
                // no-match block is the post-switch code, not a `break`ing default.
                LoOp::Jump { .. } | LoOp::Leave | LoOp::Ret { .. } => break,
                _ => {}
            }
        }
        (Vec::new(), def_idx)
    }

    /// Recognize a jump-table `switch` dispatch whose range-check branch (`ja
    /// default`) is at index `ja_idx`: `mov bx,scrut; {dec|sub bx,base}; cmp
    /// bx,N; ja default; shl bx,1; jmp cs:[bx+table]`.
    /// Decode the index-setup feeding `cmp bx,N` at `cmp_idx`, returning
    /// `(scrut, base, load_idx, is_char, is_unsigned)`. `load_idx` is the first
    /// instruction of the dispatch (where straight-line folding must stop) and
    /// `scrut` the scrutinee place. For an `int` scrutinee the 0-based table
    /// index lives directly in `bx`; for a `char`/`unsigned char` it is loaded
    /// as a byte into `al`, widened (`cbw` signed / `mov ah,0` unsigned),
    /// normalized in `ax`, then copied to `bx` (`mov bx,ax`).
    fn switch_index_setup(&self, cmp_idx: usize) -> Option<(Place, i32, usize, bool, bool)> {
        // The normalization that rebases the scrutinee to a 0-based index:
        // `dec` (base 1), `sub _,k` (base k), or none (base 0). For an `int` it
        // sits on `bx`; for a `char` it sits on `ax`, before the `mov bx,ax`.
        let (base, reg_load_idx) = match self.insns.get(cmp_idx.checked_sub(1)?)?.op {
            LoOp::Un { dst: Place::Reg(Reg::Bx), op: UnOp::Dec, operand: Place::Reg(Reg::Bx) } => {
                (1, cmp_idx - 2)
            }
            LoOp::Bin {
                dst: Place::Reg(Reg::Bx),
                op: BinOp::Sub,
                lhs: Place::Reg(Reg::Bx),
                rhs: Place::Imm(k),
            } => (k, cmp_idx - 2),
            LoOp::Load { dst: Place::Reg(Reg::Bx), .. } => (0, cmp_idx - 1),
            _ => return None,
        };
        let LoOp::Load { dst: Place::Reg(Reg::Bx), src } = self.insns.get(reg_load_idx)?.op else {
            return None;
        };
        // An `int` scrutinee: the index was loaded straight into `bx`.
        if src != Place::Reg(Reg::Ax) {
            return Some((src, base, reg_load_idx, false, false));
        }
        // A `char` scrutinee: `mov bx,ax` copied a widened byte. Walk back over
        // the (optional) `ax` normalization, the widening, and the byte load.
        let (base, widen_idx) = match self.insns.get(reg_load_idx.checked_sub(1)?)?.op {
            LoOp::Un { dst: Place::Reg(Reg::Ax), op: UnOp::Dec, operand: Place::Reg(Reg::Ax) } => {
                (1, reg_load_idx - 2)
            }
            LoOp::Bin {
                dst: Place::Reg(Reg::Ax),
                op: BinOp::Sub,
                lhs: Place::Reg(Reg::Ax),
                rhs: Place::Imm(k),
            } => (k, reg_load_idx - 2),
            _ => (0, reg_load_idx - 1),
        };
        let is_unsigned = match self.insns.get(widen_idx)?.op {
            LoOp::Promote { kind: Promote::Cbw } => false,
            LoOp::Load { dst: Place::Byte(ByteReg::Ah), src: Place::Imm(0) } => true,
            _ => return None,
        };
        let load_idx = widen_idx.checked_sub(1)?;
        let LoOp::Load { dst: Place::Byte(ByteReg::Al), src } = self.insns.get(load_idx)?.op else {
            return None;
        };
        Some((src, base, load_idx, true, is_unsigned))
    }

    fn detect_jump_table(&self, ja_idx: usize) -> Option<JumpTable> {
        let LoOp::Branch { cond, target: default_off } = self.insns.get(ja_idx)?.op else {
            return None;
        };
        if cond_to_relop(cond) != Some(RelOp::UGt) {
            return None;
        }
        let cmp_idx = ja_idx.checked_sub(1)?;
        let LoOp::Bin {
            dst: Place::Flags,
            op: BinOp::Cmp,
            lhs: Place::Reg(Reg::Bx),
            rhs: Place::Imm(n),
        } = self.insns.get(cmp_idx)?.op
        else {
            return None;
        };
        let (scrut, base, load_idx, scrut_char, scrut_unsigned) =
            self.switch_index_setup(cmp_idx)?;
        // `shl bx,1` scaling the index, then the indirect jump `jmp cs:[bx+disp]`.
        match self.insns.get(ja_idx + 1)?.op {
            LoOp::Bin {
                dst: Place::Reg(Reg::Bx),
                op: BinOp::Shl,
                lhs: Place::Reg(Reg::Bx),
                rhs: Place::Imm(1),
            } => {}
            _ => return None,
        }
        let LoOp::IndirectJump { disp } = self.insns.get(ja_idx + 2)?.op else {
            return None;
        };
        Some(JumpTable {
            load_idx,
            scrut,
            scrut_char,
            scrut_unsigned,
            base,
            n,
            default_off,
            table_disp: usize::from(disp),
        })
    }

    /// Read a jump table's `(N+1)` word entries and turn them into `(case value,
    /// body index)` pairs plus the no-match block index. An entry equal to the
    /// no-match block is a **gap** (that index has no case); consecutive equal
    /// entries are **fall-through** (case values sharing a body, which
    /// [`build_switch_arms`] renders as empty lead cases). The case bodies are
    /// laid out in value order, so the entries must be non-decreasing among the
    /// present cases — otherwise (an unexpected layout) decline.
    fn jump_table_cases(&self, jt: &JumpTable) -> Option<(Vec<(i32, usize)>, usize)> {
        let count = usize::try_from(jt.n).ok()?.checked_add(1)?;
        if jt.table_disp.checked_add(2 * count)? > self.code.len() {
            return None;
        }
        let mut cases = Vec::with_capacity(count);
        let mut prev_off: Option<usize> = None;
        for k in 0..count {
            let o = jt.table_disp + 2 * k;
            let off = usize::from(self.code[o]) | (usize::from(self.code[o + 1]) << 8);
            if off == jt.default_off {
                continue; // a gap — this index falls to the no-match block
            }
            if prev_off.is_some_and(|p| off < p) {
                return None; // bodies must be laid out in value order
            }
            prev_off = Some(off);
            cases.push((jt.base + i32::try_from(k).ok()?, self.idx_of(off)?));
        }
        if cases.is_empty() {
            return None;
        }
        Some((cases, self.idx_of(jt.default_off)?))
    }

    /// Recognize a loop-rotated `while` whose header jump is at index `i`:
    /// `jmp test; body…; test: cmp…; jcc body`. Returns the body range, the
    /// `cmp`/branch indices, and the continue index, or `None`.
    fn detect_loop(&self, i: usize, hi: usize) -> Option<(usize, usize, usize, usize, usize)> {
        let LoOp::Jump { target } = self.insns[i].op else {
            return None;
        };
        let here = self.insns[i].span.start;
        if target <= here || i + 1 >= hi {
            return None;
        }
        let body_start = self.insns[i + 1].span.start;
        let test_idx = self.idx_of(target)?;
        // Find the back-branch to the body start within the loop region.
        for k in (i + 1)..hi {
            if let LoOp::Branch { target: bt, .. } = self.insns[k].op
                && bt == body_start
            {
                // The branch reads the `cmp` just before it; the header jump
                // lands at the start of the test region, which may include a
                // register-setup load before the `cmp` (e.g. `mov ax,i; cmp
                // ax,n` when comparing two memory operands). So the test region
                // is `[test_idx, k)` with the `cmp` at `k-1`.
                if k > 0 && test_idx < k {
                    return Some((i + 1, test_idx, k - 1, k, k + 1));
                }
                return None;
            }
        }
        None
    }

    /// Structure the instruction range `[lo, hi)` into statements, recovering
    /// nested `if`/`while`. Assumes the accumulator is empty at the boundaries
    /// (BCC flushes to memory before any branch).
    #[allow(clippy::too_many_lines)] // a flat per-shape match reads better unsplit
    fn structure(&mut self, lo: usize, hi: usize) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        let mut i = lo;
        let mut linear_start = lo;
        while i < hi {
            // Copy out the Copy-able control fields so the immutable borrow ends
            // before we recurse / fold (which need `&mut self`).
            let ctrl = match self.insns[i].op {
                LoOp::Branch { target, .. } => Some((true, target)),
                LoOp::Jump { target } => Some((false, target)),
                _ => None,
            };
            let here = self.insns[i].span.start;

            match ctrl {
                Some((true, target)) if target > here => {
                    // Forward conditional branch → an `if` (or a `switch`). The
                    // `cmp` feeding it is the previous instruction; everything
                    // before that is a straight-line run.
                    if i == 0 {
                        self.cant_for("structure:if-at-start");
                        i += 1;
                        continue;
                    }
                    let cmp_idx = i - 1;

                    // A `switch`, two shapes. A compare-chain (`cmp ax,Ki; je Ti`
                    // × N) — the scrutinee is the value the run left in `ax`. Or a
                    // jump table (`cmp bx,N; ja default; jmp cs:[bx+table]`) — the
                    // scrutinee is the value loaded into `bx`, read from the
                    // embedded offset table.
                    if let Some((cases, def_idx)) = self.detect_switch(cmp_idx) {
                        let resume = if let Some(scrutinee) =
                            self.fold_linear(linear_start, cmp_idx, &mut stmts)
                        {
                            self.emit_switch(&mut stmts, scrutinee, &cases, def_idx, hi)
                        } else {
                            self.cant_for("structure:switch-scrutinee");
                            def_idx
                        };
                        i = resume;
                        linear_start = resume;
                        continue;
                    }
                    if let Some(jt) = self.detect_jump_table(i) {
                        if let Some((cases, def_idx)) = self.jump_table_cases(&jt) {
                            self.fold_linear(linear_start, jt.load_idx, &mut stmts);
                            let resume = if let Some(scrutinee) = self.operand(jt.scrut) {
                                if jt.scrut_char {
                                    self.mark_char(&scrutinee);
                                }
                                if jt.scrut_unsigned {
                                    self.mark_unsigned(&scrutinee);
                                }
                                self.emit_switch(&mut stmts, scrutinee, &cases, def_idx, hi)
                            } else {
                                self.cant_for("structure:jumptable-scrutinee");
                                def_idx
                            };
                            i = resume;
                            linear_start = resume;
                            continue;
                        }
                        // A jump table we can't fully read (out-of-order layout).
                        self.cant_for("structure:jumptable-layout");
                        i += 1;
                        continue;
                    }

                    // The fold returns the value left in the accumulator (e.g.
                    // `*p` set up before an `or ax,ax` / `cmp ax,K`), which the
                    // condition needs to read a register operand.
                    let cond_acc = self.fold_linear(linear_start, cmp_idx, &mut stmts);
                    let Some(tb) = self.idx_of(target) else {
                        self.cant_for("structure:branch-target");
                        i += 1;
                        continue;
                    };
                    let cond = self.condition(cmp_idx, i, true, cond_acc.as_ref());
                    let e_idx_opt = self.else_end(tb, target);
                    // A ternary `cond ? t : f` — an if/else whose both arms reduce
                    // to a single accumulator value (no statements), converging at
                    // the merge. The merged value is seeded for the consumer that
                    // follows (a `return`/store).
                    if let Some(e_idx) = e_idx_opt
                        && let Some(tern) = self.detect_ternary(i + 1, tb, e_idx, &cond)
                    {
                        self.pending_acc = Some(tern);
                        i = e_idx;
                        linear_start = e_idx;
                        continue;
                    }
                    // `if/else` when the then-block ends in a jump past the
                    // else-block; otherwise a plain `if`.
                    let resume = if let Some(e_idx) = e_idx_opt {
                        let then = self.structure(i + 1, tb - 1);
                        let otherwise = self.structure(tb, e_idx);
                        stmts.push(Stmt::If(cond, then, otherwise));
                        e_idx
                    } else {
                        let then = self.structure(i + 1, tb);
                        stmts.push(Stmt::If(cond, then, Vec::new()));
                        tb
                    };
                    i = resume;
                    linear_start = resume;
                }
                Some((false, _)) => {
                    // An unconditional jump: a `while` header, or the exit jump.
                    if let Some((b_lo, b_hi, cmp_idx, br_idx, cont)) = self.detect_loop(i, hi) {
                        self.fold_linear(linear_start, i, &mut stmts);
                        // Fold the test region `[b_hi, cmp_idx)` (the `cmp`'s
                        // register setup, e.g. `mov ax,i`) into a throwaway buffer
                        // to recover the accumulator the condition reads.
                        let mut test_setup = Vec::new();
                        let cond_acc = self.fold_linear(b_hi, cmp_idx, &mut test_setup);
                        let cond = self.condition(cmp_idx, br_idx, false, cond_acc.as_ref());
                        let body = self.structure(b_lo, b_hi);
                        stmts.push(Stmt::While(cond, body));
                        i = cont;
                        linear_start = cont;
                    } else {
                        i += 1; // exit jump — folded (ignored) with the linear run
                    }
                }
                Some((true, target)) => {
                    // A backward conditional branch with no header jump is a
                    // `do { body } while (cond)`: the body runs, then the branch
                    // loops back to the body start when the condition holds.
                    let cmp_idx = i - 1;
                    if i > 0
                        && let Some(body_start) = self.idx_of(target)
                        && body_start <= cmp_idx
                    {
                        // Everything before the body is pre-loop init.
                        self.fold_linear(linear_start, body_start, &mut stmts);
                        // Fold the body straight-line; its trailing accumulator is
                        // the `cmp`'s operand setup, which the condition reads.
                        let mut body = Vec::new();
                        let cond_acc = self.fold_linear(body_start, cmp_idx, &mut body);
                        let cond = self.condition(cmp_idx, i, false, cond_acc.as_ref());
                        stmts.push(Stmt::Do(cond, body));
                        i += 1;
                        linear_start = i;
                    } else {
                        self.cant_for("structure:control-flow");
                        i += 1;
                    }
                }
                None => i += 1,
            }
        }
        self.fold_linear(linear_start, hi, &mut stmts);
        stmts
    }
}

/// Recover a function body from `_TEXT` bytes: lift to Lo-IR, then structure and
/// fold it into statements. Treats the whole `_TEXT` as one function — see
/// [`recover_program`] for a multi-function segment.
#[must_use]
pub fn recover(code: &[u8]) -> Function {
    let insns: Vec<LoInsn> = lift(code);
    recover_window(&insns, code, 0, insns.len())
}

/// Recover every function in a `_TEXT` segment. BCC lays functions out one after
/// another, each beginning with a prologue (`Enter`); a near `call` to a local
/// function lands on that prologue. So the function boundaries are the `Enter`
/// positions, and each window is recovered independently with absolute offsets
/// preserved (the lift keeps byte offsets, so branch/call targets and embedded
/// jump tables still resolve against the full segment).
///
/// A lone function (or a segment with no recognizable prologue) yields a
/// one-element vector equal to [`recover`].
#[must_use]
pub fn recover_program(code: &[u8]) -> Vec<Function> {
    let insns: Vec<LoInsn> = lift(code);
    // A function starts at a prologue `Enter`, but not *every* `Enter` is one:
    // BCC reserves a 2-byte local with `dec sp; dec sp`, which also lifts to an
    // `Enter` (frame 2) mid-prologue. The real boundary is an `Enter` that opens
    // a fresh function — the first one, and any that follows the previous
    // function's `ret`. So only count an `Enter` once a `Ret` has been seen since
    // the last accepted start.
    let mut starts = Vec::new();
    let mut seen_ret = true; // the first prologue qualifies
    for (i, n) in insns.iter().enumerate() {
        match n.op {
            LoOp::Enter { .. } if seen_ret => {
                starts.push(i);
                seen_ret = false;
            }
            LoOp::Ret { .. } => seen_ret = true,
            _ => {}
        }
    }
    if starts.len() <= 1 {
        return vec![recover_window(&insns, code, 0, insns.len())];
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &lo)| {
            let hi = starts.get(k + 1).copied().unwrap_or(insns.len());
            recover_window(&insns, code, lo, hi)
        })
        .collect()
}

/// Recover the single function occupying instruction window `[lo, hi)` of `all`.
/// `code` is the *full* segment (not the window's slice) so embedded jump tables
/// — addressed by absolute offset — still read correctly.
fn recover_window(all: &[LoInsn], code: &[u8], lo: usize, hi: usize) -> Function {
    let insns: Vec<LoInsn> = all[lo..hi].to_vec();
    let start = insns.first().map_or(0, |i| i.span.start);
    // Pre-scan for `long` locals: a slot read back as a `dx:ax` pair
    // (`mov dx,[lo+2]; mov ax,[lo]`). This lets the store side fold a `long`
    // constant store pair without mistaking two adjacent `int` stores for one.
    let mut long_slots = std::collections::HashSet::new();
    for pair in insns.windows(2) {
        if let (
            LoOp::Load { dst: Place::Reg(Reg::Dx), src: Place::Local(hi) },
            LoOp::Load { dst: Place::Reg(Reg::Ax), src: Place::Local(lo) },
        ) = (&pair[0].op, &pair[1].op)
            && *hi == lo + 2
        {
            long_slots.insert(*lo);
        }
    }
    // Pre-scan for word-accessed slots/globals — an `int` (or wider). A byte load
    // of such a slot (`mov al,[n]`) is reading the low byte of an `int`, not a
    // `char`, so the char-marking that load does must be undone afterward. (Byte
    // accesses use `Byte(_)`/`StoreImmByte`; a full-register or word-immediate
    // store/load is what marks an `int`.)
    let mut word_slots: std::collections::HashSet<i16> = std::collections::HashSet::new();
    let mut word_globals: std::collections::HashSet<u16> = std::collections::HashSet::new();
    for insn in &insns {
        match &insn.op {
            LoOp::Store { dst: Place::Local(o), src: Place::Reg(_) | Place::Imm(_) }
            | LoOp::Load { dst: Place::Reg(_), src: Place::Local(o) } => {
                word_slots.insert(*o);
            }
            LoOp::Store { dst: Place::Global(a), src: Place::Reg(_) | Place::Imm(_) }
            | LoOp::Load { dst: Place::Reg(_), src: Place::Global(a) } => {
                word_globals.insert(*a);
            }
            _ => {}
        }
    }
    let mut ctx = Ctx {
        insns,
        code: code.to_vec(),
        vars: Vec::new(),
        char_vars: Vec::new(),
        ptr_vars: Vec::new(),
        char_ptr_vars: Vec::new(),
        long_vars: Vec::new(),
        unsigned_vars: Vec::new(),
        long_slots,
        word_slots,
        word_globals,
        char_array_bases: Vec::new(),
        long_array_bases: Vec::new(),
        complete: true,
        returns_value: false,
        returns_long: false,
        returns_char: false,
        pending_acc: None,
        cur: 0,
        reason: None,
    };
    // Structure up to the last `ret` — a jump-table `switch` appends its offset
    // table after the epilogue, and that data isn't code.
    let len = ctx
        .insns
        .iter()
        .rposition(|n| matches!(n.op, LoOp::Ret { .. }))
        .map_or(ctx.insns.len(), |r| r + 1);
    let mut body = fold_for_loops(ctx.structure(0, len));
    promote_params(&mut ctx, &mut body);
    coalesce_int_pointer_increments(&ctx, &mut body);
    // A `long` occupies two slots (lo, lo+2). If the high-word slot was also
    // recovered as a separate variable (e.g. a `long` local's paired stores read
    // as two `int` stores), the layout is double-counted — bail rather than emit
    // it. (This guards the deferred `long`-local store-pairing case.)
    let double_counted = ctx.long_vars.iter().any(|&lv| {
        let high = match lv {
            Var::Slot(lo) => Some(Var::Slot(lo + 2)),
            Var::Param(lo) => Some(Var::Param(lo + 2)),
            _ => None,
        };
        high.is_some_and(|h| ctx.vars.contains(&h))
    });
    if double_counted {
        ctx.cant_for("long-high-slot-double-count");
    }
    let ret = if ctx.returns_long {
        Type::Long
    } else if ctx.returns_char {
        Type::Char
    } else if ctx.returns_value {
        Type::Int
    } else {
        Type::Void
    };
    let arrays = detect_local_array(&ctx);
    // If a variable-indexed array element was folded (`&a[0] + i`) but the frame
    // pass couldn't reconstruct that array, the body holds a dangling `&v[i]` —
    // bail rather than emit it. (Happens for a `long` array that also has a
    // constant-index store-pair, which isn't folded into the array yet.)
    let covered = |base: i16| arrays.iter().any(|a| a.base == base);
    let dangling_array =
        !ctx.char_array_bases.iter().chain(&ctx.long_array_bases).all(|&b| covered(b));
    let complete = ctx.complete && !dangling_array;
    let bail_reason = ctx
        .reason
        .or_else(|| dangling_array.then(|| "dangling-array".to_string()));
    Function {
        ret,
        vars: ctx.vars,
        char_vars: ctx.char_vars,
        ptr_vars: ctx.ptr_vars,
        char_ptr_vars: ctx.char_ptr_vars,
        long_vars: ctx.long_vars,
        unsigned_vars: ctx.unsigned_vars,
        arrays,
        body,
        complete,
        bail_reason,
        start,
    }
}

/// A short, stable signature of a lo-IR op for bail-reason triage — the variant
/// plus just enough operand shape to cluster the long tail (which `Bin`, whether
/// a `Load`/`Store` touches a deref/global, …). Not exhaustive; tuned to separate
/// the recovery gaps worth chasing.
fn op_sig(op: &LoOp) -> &'static str {
    match op {
        LoOp::Asm { .. } => "Asm(unlifted)",
        LoOp::Bin { op, .. } => match op {
            BinOp::Add => "Bin:Add",
            BinOp::Sub => "Bin:Sub",
            BinOp::And => "Bin:And",
            BinOp::Or => "Bin:Or",
            BinOp::Xor => "Bin:Xor",
            BinOp::Mul | BinOp::Imul => "Bin:Mul",
            BinOp::Idiv | BinOp::Div => "Bin:Div",
            BinOp::Shl => "Bin:Shl",
            BinOp::Shr | BinOp::Sar => "Bin:Shr",
            BinOp::Adc => "Bin:Adc",
            BinOp::Sbb => "Bin:Sbb",
            BinOp::Cmp => "Bin:Cmp",
            _ => "Bin:other",
        },
        LoOp::Un { op, .. } | LoOp::UnByte { op, .. } => match op {
            UnOp::Inc => "Un:Inc",
            UnOp::Dec => "Un:Dec",
            UnOp::Neg => "Un:Neg",
            UnOp::Not => "Un:Not",
        },
        LoOp::Load { src: Place::Deref(_) | Place::DerefDisp(..), .. } => "Load:deref",
        LoOp::Load { src: Place::Global(_), .. } => "Load:global",
        LoOp::Load { .. } => "Load",
        LoOp::Store { dst: Place::Deref(_) | Place::DerefDisp(..), .. } => "Store:deref",
        LoOp::Store { dst: Place::Global(_), .. } => "Store:global",
        LoOp::Store { .. } => "Store",
        LoOp::StoreImmByte { .. } => "StoreImmByte",
        LoOp::CmpByte { .. } => "CmpByte",
        LoOp::Lea { .. } => "Lea",
        LoOp::Arg { .. } => "Arg",
        LoOp::Call { .. } => "Call",
        LoOp::Cleanup { .. } => "Cleanup",
        LoOp::Branch { .. } => "Branch",
        LoOp::Jump { .. } => "Jump",
        LoOp::IndirectJump { .. } => "IndirectJump",
        LoOp::Pop { .. } => "Pop",
        LoOp::Promote { .. } => "Promote",
        LoOp::Enter { .. } | LoOp::Leave | LoOp::Ret { .. } => "frame",
        LoOp::SaveReg { .. } | LoOp::RestoreReg { .. } => "regsave",
    }
}

/// Reconstruct a sole local `int` array from the frame, or `[]` for a plain
/// scalar layout. A constant array index folds to a direct `[bp+disp]` slot, so
/// `int a[M]` surfaces as scalar slots — but only the *accessed* ones, which
/// under-allocates the frame. When the recovered `int` slots *are* the whole
/// top-packed scalar layout (offsets `-2,-4,…,-2k` reaching the `Enter` frame),
/// they're genuine scalars and stay so. Otherwise the frame is modelled as one
/// `int` array spanning it: a slot at `off` becomes `a[(off+N)/2]`, which
/// reproduces the very same `[bp+disp]` access, so the array always round-trips.
///
/// Scoped to all-`int` frames (no `char`/`long`/pointer slot, whose widths make
/// the layout subtler) — those keep today's scalar recovery.
fn detect_local_array(ctx: &Ctx) -> Vec<ArraySpec> {
    let frame = ctx.insns.iter().find_map(|i| match i.op {
        LoOp::Enter { frame } => Some(i16::try_from(frame).unwrap_or(0)),
        _ => None,
    });
    let Some(frame) = frame else { return Vec::new() };
    if frame < 2 || frame % 2 != 0 {
        return Vec::new();
    }
    // The element type a slot offset belongs to: `long` (a `dx:ax` pair — a long
    // slot, or a long-array base), `char` (byte access), else `int`.
    let elem_of = |off: i16| -> ArrayElem {
        let v = Var::Slot(off);
        if ctx.long_vars.contains(&v) || ctx.long_array_bases.contains(&off) {
            ArrayElem::Long
        } else if ctx.char_vars.contains(&v) || ctx.char_array_bases.contains(&off) {
            ArrayElem::Char
        } else {
            ArrayElem::Int
        }
    };
    let mut slots: Vec<i16> = Vec::new();
    for v in &ctx.vars {
        if let Var::Slot(off) = v {
            if ctx.ptr_vars.contains(v) || ctx.char_ptr_vars.contains(v) {
                return Vec::new();
            }
            if *off >= 0 || *off < -frame {
                return Vec::new();
            }
            slots.push(*off);
        }
    }
    if slots.is_empty() {
        return Vec::new();
    }
    // The determinate signal: a `lea` of a frame slot is an array base (its
    // address is taken / it's variable-indexed). Partition the frame on those
    // anchors — each lea-array runs from its base up to the next boundary (the
    // next-higher accessed slot or lea base), at the element's stride. This
    // splits genuine mixed frames (`int x; int a[4]`) that the sole-array
    // fallback below would otherwise merge.
    let mut lea_bases: Vec<i16> = ctx
        .insns
        .iter()
        .filter_map(|i| match i.op {
            LoOp::Lea { src: Place::Local(off), .. } if off < 0 && off >= -frame => Some(off),
            _ => None,
        })
        .collect();
    lea_bases.sort_unstable();
    lea_bases.dedup();
    if !lea_bases.is_empty() {
        // Boundaries: every accessed slot and lea base, plus the frame top (0).
        let mut bounds: Vec<i16> = slots.iter().chain(lea_bases.iter()).copied().collect();
        bounds.push(0);
        bounds.sort_unstable();
        bounds.dedup();
        let mut arrays: Vec<ArraySpec> = Vec::new();
        for &base in &lea_bases {
            let elem = elem_of(base);
            let next = bounds.iter().copied().find(|&b| b > base).unwrap_or(0);
            let len = u16::try_from((next - base) / elem.stride()).unwrap_or(0);
            // A single-element "array" is an address-taken scalar, not an array.
            if len >= 2 {
                arrays.push(ArraySpec { base, len, elem });
            }
        }
        if !arrays.is_empty() {
            return arrays;
        }
    }
    // The constant-index sole-array / scalar paths handle `int` and `char` only —
    // a `long` slot here (a `dx:ax` store-pair or read) involves the long
    // machinery and isn't modelled, so bail (sound) rather than mis-shape it.
    let any_char = slots.iter().any(|&o| elem_of(o) == ArrayElem::Char);
    let any_int = slots.iter().any(|&o| elem_of(o) == ArrayElem::Int);
    let any_long = slots.iter().any(|&o| elem_of(o) == ArrayElem::Long);
    if any_long || (any_char && any_int) {
        return Vec::new();
    }
    let elem = if any_char { ArrayElem::Char } else { ArrayElem::Int };
    let stride = elem.stride();
    if elem == ArrayElem::Int && slots.iter().any(|o| o % 2 != 0) {
        return Vec::new(); // an int array can't sit on an odd offset
    }
    // Genuine top-packed scalars: offsets are exactly {-s,-2s,…,-ks} and fill the
    // frame (a `char` frame is padded up to even). Keep them as scalars — merging
    // them into one `int` array would discard each slot's signedness (an array is a
    // single element type), which silently changes unsigned div/shift/compare
    // codegen. (So a constant-index `int a[N]` with no `lea` anchor stays a known
    // gap; distinguishing it from spilled scalars needs per-slot type evidence.)
    slots.sort_unstable_by(|a, b| b.cmp(a)); // descending toward bp
    let k = i16::try_from(slots.len()).unwrap_or(0);
    let top_packed = slots.iter().enumerate().all(|(j, &o)| o == -stride * (i16::try_from(j).unwrap_or(0) + 1));
    let scalar_frame = if elem == ArrayElem::Char { (k + 1) & !1 } else { 2 * k };
    if top_packed && scalar_frame == frame {
        return Vec::new();
    }
    // No array anchor and not a scalar layout — model the frame as one array.
    vec![ArraySpec { base: -frame, len: u16::try_from(frame / stride).unwrap_or(0), elem }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{recompile_text, CompileOpts};

    fn recover_c(src: &str) -> Function {
        let code = recompile_text(src, &CompileOpts::default()).expect("compiles");
        recover(&code)
    }

    /// Stack-local options (`-r-`): this increment recovers stack locals, not the
    /// `si`/`di` register variables BCC allocates by default.
    fn stack_opts() -> CompileOpts {
        CompileOpts { no_reg_vars: true, ..CompileOpts::default() }
    }

    fn recover_stack(src: &str) -> Function {
        let code = recompile_text(src, &stack_opts()).expect("compiles");
        recover(&code)
    }

    #[test]
    fn return_const_folds_to_return_expr() {
        let f = recover_c("int f() { return 42; }\n");
        assert!(f.complete);
        assert_eq!(f.ret, Type::Int);
        assert_eq!(f.body, vec![Stmt::Return(Some(Expr::Const(42)))]);
    }

    #[test]
    fn a_sparse_int_array_is_modelled_as_an_array() {
        // `int a[4]` accessed only at a[0] and a[2]: frame 8, but the two
        // accessed slots can't be the whole top-packed scalar layout (which would
        // be a frame-4 pair at -2/-4), so the frame is one `int a[4]`.
        let f = recover_stack("int f() { int a[4]; a[0] = 1; return a[2]; }\n");
        assert!(f.complete);
        assert_eq!(f.arrays.len(), 1, "one reconstructed array");
        assert_eq!(f.arrays[0], ArraySpec { base: -8, len: 4, elem: ArrayElem::Int });
    }

    #[test]
    fn genuine_scalars_are_not_arrayed() {
        // Two `int` locals are a tight top-packed layout (-2, -4, frame 4) — they
        // stay scalars, not a 2-element array.
        let f = recover_stack("int f() { int x; int y; x = 3; y = 4; return x + y; }\n");
        assert!(f.complete && f.arrays.is_empty());
    }

    #[test]
    fn a_lea_anchor_partitions_a_mixed_frame() {
        // `int x; int a[4]` with `a[i]`: the `lea` of a's base (-10) anchors a
        // 4-element array there; the scalar x at -2 stays out of it — not one
        // merged a[5].
        let f = recover_stack("int f(int i) { int x; int a[4]; x = 9; return x + a[i]; }\n");
        assert!(f.complete);
        assert_eq!(f.arrays, vec![ArraySpec { base: -10, len: 4, elem: ArrayElem::Int }]);
    }

    #[test]
    fn a_char_array_is_stride_one() {
        // Byte accesses (`char a[4]`) reconstruct a stride-1 array; a
        // variable-only-indexed array is typed `char` from the byte deref alone.
        let f = recover_stack("int f() { char a[4]; a[0] = 65; return a[2]; }\n");
        assert_eq!(f.arrays, vec![ArraySpec { base: -4, len: 4, elem: ArrayElem::Char }]);
        let g = recover_stack("int f(int i) { char a[8]; return a[i]; }\n");
        assert_eq!(g.arrays, vec![ArraySpec { base: -8, len: 8, elem: ArrayElem::Char }]);
    }

    #[test]
    fn a_long_array_is_stride_four() {
        // A variable-indexed `long` array: stride 4, element type from the
        // `dx:ax` pair deref (`mov dx,[bx+2]; mov ax,[bx]`) with no slot evidence.
        let f = recover_stack("long f(int i) { long a[8]; return a[i]; }\n");
        assert_eq!(f.arrays, vec![ArraySpec { base: -32, len: 8, elem: ArrayElem::Long }]);
    }

    #[test]
    fn a_long_array_store_is_sound_not_wrong() {
        // A `long` array *store* writes only the low word (a BCC quirk), so it
        // isn't a clean `long` assignment — the recovery declines rather than
        // mis-shape it as an `int` array (which the stride-4 index would betray).
        let f = recover_stack("void f(int i, long v) { long a[8]; a[i] = v; }\n");
        assert!(!f.complete, "the long-array store is declined, not mis-recovered");
    }

    #[test]
    fn an_address_taken_scalar_is_not_an_array() {
        // `&x` is a `lea` too, but its one-element span isn't an array — `x` stays
        // a scalar.
        let f = recover_stack("int f() { int x; int *p; x = 3; p = &x; return *p; }\n");
        assert!(f.complete && f.arrays.is_empty());
    }

    #[test]
    fn assign_then_return_folds_through_the_slot() {
        // `-r-` keeps `x` on the stack, so the variable is a slot.
        let f = recover_stack("int f() { int x; x = 5; return x; }\n");
        assert!(f.complete, "straight-line int code is fully recovered");
        assert_eq!(f.vars.len(), 1, "one variable");
        let var = f.vars[0];
        assert!(matches!(var, Var::Slot(d) if d < 0), "a stack slot below bp");
        assert_eq!(
            f.body,
            vec![
                Stmt::Assign(LValue::Var(var), Expr::Const(5)),
                Stmt::Return(Some(Expr::Var(var))),
            ],
        );
    }

    #[test]
    fn register_variable_is_recovered_from_default_codegen() {
        // BCC promotes a sufficiently-used local to `si` (a single store+load
        // stays on the stack; a variable read in a loop is promoted). The
        // accumulator routing (`mov ax,si` / `mov si,ax`) and the `xor si,si`
        // zero idiom must recover it as an ordinary variable.
        let f = recover_c("int f() { int x; x = 0; while (x < 10) { x = x + 1; } return x; }\n");
        assert!(f.complete, "a register-variable function is recovered");
        assert_eq!(f.vars.len(), 1, "one variable");
        assert!(matches!(f.vars[0], Var::Reg(Reg::Si)), "x lives in si");
        // x = 0 (xor si,si), the loop, then return x (mov ax,si).
        assert!(matches!(f.body.first(), Some(Stmt::Assign(_, Expr::Const(0)))));
        assert!(matches!(f.body.last(), Some(Stmt::Return(Some(Expr::Var(Var::Reg(Reg::Si)))))));
    }

    #[test]
    fn register_variable_truthiness_test_recovers() {
        // `if (x)` on a register variable is `or si,si; jz` — the truthiness
        // idiom recovers as the *bare* variable (modelling it as `x != 0` would
        // recompile to a `cmp`, not the original `or`).
        let f = recover_c("int f() { int x; x = 0; if (x) { x = 1; } return x; }\n");
        assert!(f.complete, "register-variable if is recovered");
        let Stmt::If(cond, ..) = f.body.iter().find(|s| matches!(s, Stmt::If(..))).expect("an if")
        else {
            unreachable!()
        };
        assert_eq!(*cond, Expr::Var(Var::Reg(Reg::Si)), "or si,si; jz → bare `x`");
    }

    #[test]
    fn if_recovers_a_negated_branch_condition() {
        // `if (x == 5)` compiles to `cmp x,5; jne skip`, so the condition is the
        // negation of the branch (jne → ==).
        let f = recover_stack("int f() { int x; x = 3; if (x == 5) { x = 7; } return x; }\n");
        assert!(f.complete, "stack-local if is recovered");
        let if_stmt = f.body.iter().find(|s| matches!(s, Stmt::If(..))).expect("an if");
        let Stmt::If(Expr::Rel(op, _, rhs), then, els) = if_stmt else {
            panic!("expected an if with a relational condition");
        };
        assert_eq!(*op, RelOp::Eq, "jne negated back to ==");
        assert_eq!(**rhs, Expr::Const(5));
        assert_eq!(then.len(), 1, "one then-statement");
        assert!(els.is_empty(), "no else");
    }

    #[test]
    fn if_else_recovers_both_arms() {
        let f =
            recover_stack("int f() { int x; x = 3; if (x == 5) { x = 7; } else { x = 9; } return x; }\n");
        assert!(f.complete);
        let Stmt::If(_, then, els) = f.body.iter().find(|s| matches!(s, Stmt::If(..))).unwrap() else {
            unreachable!()
        };
        assert_eq!(then.len(), 1, "then arm");
        assert_eq!(els.len(), 1, "else arm");
    }

    #[test]
    fn for_loop_recovers_for_structure() {
        // A `for` lowers to `init; while (cond) { body; step }`, and a `while`
        // with that exact shape (loop var initialized before, stepped at the
        // tail) recovers as a `for`.
        let f = recover_stack(
            "int f() { int s; int i; s = 0; for (i = 0; i < 10; i = i + 1) { s = s + i; } return s; }\n",
        );
        assert!(f.complete, "the for-loop is recovered");
        let Some(Stmt::For(init, Expr::Rel(op, _, rhs), step, body)) =
            f.body.iter().find(|s| matches!(s, Stmt::For(..)))
        else {
            panic!("expected a for-loop");
        };
        assert!(matches!(**init, Stmt::Assign(..)), "init is the loop-var assignment");
        assert!(matches!(**step, Stmt::Assign(..)), "step is the loop-var update");
        assert_eq!(*op, RelOp::Lt);
        assert_eq!(**rhs, Expr::Const(10));
        assert_eq!(body.len(), 1, "body without the step is `s = s + i`");
    }

    #[test]
    fn loop_condition_comparing_two_memory_operands_recovers() {
        // `i < n` (local vs parameter) compiles to `mov ax,i; cmp ax,n` — the
        // accumulator operand resolves to `i` from the preceding load.
        let f = recover_stack(
            "int f(int n) { int i; int s; s = 0; for (i = 0; i < n; i = i + 1) { s = s + i; } return s; }\n",
        );
        assert!(f.complete, "a loop comparing a local to a parameter is recovered");
        let Some(Stmt::For(_, Expr::Rel(_, lhs, rhs), ..)) =
            f.body.iter().find(|s| matches!(s, Stmt::For(..)))
        else {
            panic!("expected a for-loop");
        };
        assert!(matches!(**lhs, Expr::Var(Var::Slot(_))), "lhs is the local i");
        assert_eq!(**rhs, Expr::Var(Var::Param(4)), "rhs is the parameter n");
    }

    #[test]
    fn while_recovers_loop_rotation() {
        // `while (x < 10)` is loop-rotated; the back-branch condition (jl → <) is
        // the loop-continue condition, taken verbatim (not negated).
        let f = recover_stack("int f() { int x; x = 0; while (x < 10) { x = x + 1; } return x; }\n");
        assert!(f.complete, "stack-local while is recovered");
        let Stmt::While(Expr::Rel(op, _, rhs), body) =
            f.body.iter().find(|s| matches!(s, Stmt::While(..))).expect("a while")
        else {
            panic!("expected a while with a relational condition");
        };
        assert_eq!(*op, RelOp::Lt, "jl → <, not negated");
        assert_eq!(**rhs, Expr::Const(10));
        assert_eq!(body.len(), 1, "one body statement (x = x + 1)");
    }

    #[test]
    fn call_recovers_with_arguments_and_a_parameter() {
        // `g(a)` — a parameter passed to a call. The parameter is `[bp+4]`, the
        // call result is returned.
        let f = recover_c("extern int g(); int f(int a) { return g(a); }\n");
        assert!(f.complete, "a call with a parameter argument is recovered");
        assert!(f.vars.contains(&Var::Param(4)), "the parameter is [bp+4]");
        let Some(Stmt::Return(Some(Expr::Call(_, args)))) = f.body.last() else {
            panic!("expected `return g(a)`");
        };
        assert_eq!(args.len(), 1, "one argument");
        assert_eq!(args[0], Expr::Var(Var::Param(4)), "the argument is the parameter");
    }

    #[test]
    fn discarded_call_recovers_as_a_statement() {
        // `g(3); g(4);` — the first call's result is discarded, so it must
        // surface as its own statement, not be dropped.
        let f = recover_c("extern void g(); void f() { g(3); g(4); }\n");
        assert!(f.complete);
        assert!(
            matches!(f.body.first(), Some(Stmt::ExprStmt(Expr::Call(..)))),
            "the discarded first call is an expression statement",
        );
    }

    #[test]
    fn near_globals_recover_by_offset() {
        // Two distinct globals are told apart by their data-segment offset
        // (`a`@0, `b`@2) — the displacement is real, not a placeholder.
        let f = recover_c("int a; int b; int f() { a = b; return a; }\n");
        assert!(f.complete, "scalar near globals are recovered");
        assert!(f.vars.contains(&Var::Global(0)), "a is at offset 0");
        assert!(f.vars.contains(&Var::Global(2)), "b is at offset 2");
    }

    #[test]
    fn char_width_is_recovered_from_byte_access() {
        // A byte-accessed global is a `char` — the `a0` load + `cbw` promotion.
        let f = recover_c("char cv; int f() { return cv; }\n");
        assert!(f.complete, "a char global is recovered");
        assert!(f.char_vars.contains(&Var::Global(0)), "byte access marks it char");
        assert_eq!(f.body, vec![Stmt::Return(Some(Expr::Var(Var::Global(0))))]);
    }

    #[test]
    fn char_register_variable_is_recovered() {
        // BCC promotes `c` to the `dl` byte register variable; it recovers as a
        // char variable (always char — a byte register holds a char).
        let f = recover_c("int f() { char c; c = 0; if (c == 0) { c = 1; } return c; }\n");
        assert!(f.complete, "a char register variable is recovered");
        assert!(matches!(f.vars[0], Var::ByteReg(ByteReg::Dl)), "c lives in dl");
        assert!(f.char_vars.contains(&Var::ByteReg(ByteReg::Dl)), "a byte reg var is char");
    }

    #[test]
    fn multiply_and_modulo_recover() {
        // `a * b` is imul (low word = the int product); `a % b` is idiv then
        // `mov ax,dx` (the remainder).
        let m = recover_stack("int f(int a, int b) { return a * b; }\n");
        assert!(m.complete);
        let Some(Stmt::Return(Some(Expr::Binary(op, ..)))) = m.body.last() else {
            panic!("expected `return a * b`");
        };
        assert_eq!(*op, BinOp::Imul);

        let r = recover_stack("int f(int a, int b) { return a % b; }\n");
        assert!(r.complete);
        let Some(Stmt::Return(Some(Expr::Binary(op, ..)))) = r.body.last() else {
            panic!("expected `return a % b`");
        };
        assert_eq!(*op, BinOp::Mod, "idiv remainder via mov ax,dx is `%`");
    }

    #[test]
    fn unsigned_comparison_recovers_and_marks_operands_unsigned() {
        // `a > 5` (unsigned) compiles to `cmp a,5; jbe` — the unsigned branch
        // marks `a` unsigned so it re-emits as `jbe`, not `jg`.
        let f = recover_stack("int f(unsigned a) { if (a > 5) { return 1; } return 0; }\n");
        assert!(f.complete, "an unsigned comparison is recovered");
        assert!(f.unsigned_vars.contains(&Var::Param(4)), "the compared operand is unsigned");
        let Some(Stmt::If(Expr::Rel(op, ..), ..)) = f.body.iter().find(|s| matches!(s, Stmt::If(..)))
        else {
            panic!("expected an unsigned if");
        };
        assert_eq!(*op, RelOp::UGt, "jbe negated → unsigned >");
    }

    #[test]
    fn unsigned_char_recovers() {
        // `unsigned char` zero-extends with `mov ah,0` (not `cbw`) — the char is
        // marked both `char` and `unsigned`.
        let f = recover_stack("int f(unsigned char c) { return c; }\n");
        assert!(f.complete, "an unsigned char is recovered");
        assert!(f.char_vars.contains(&Var::Param(4)), "byte access → char");
        assert!(f.unsigned_vars.contains(&Var::Param(4)), "mov ah,0 zero-extend → unsigned");
    }

    #[test]
    fn long_arithmetic_recovers() {
        // `a + b` for longs is `add ax,[b.lo]; adc dx,[b.hi]`; a negative long
        // constant normalizes to a subtraction.
        let f = recover_stack("long f(long a, long b) { return a + b; }\n");
        assert!(f.complete, "long add is recovered");
        assert_eq!(f.ret, Type::Long);
        assert!(f.long_vars.contains(&Var::Param(4)) && f.long_vars.contains(&Var::Param(8)));
    }

    #[test]
    fn a_compare_chain_switch_recovers() {
        // A small switch (≤ 3 cases) is a compare-chain (`cmp ax,K; je case`),
        // which recovers as a `Stmt::Switch`.
        let f =
            recover_c("int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }\n");
        assert!(f.complete, "a compare-chain switch is recovered");
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Switch(_, arms, _) if arms.len() == 3)),
            "recovers a 3-arm switch",
        );
    }

    #[test]
    fn a_distinct_jump_table_switch_recovers() {
        // A dense switch with distinct bodies (≥ 4 contiguous cases) is a jump
        // table (`cmp bx,N; ja default; jmp cs:[bx+table]`) read from the table.
        let f = recover_c(
            "int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; case 4: return 4; } return 0; }\n",
        );
        assert!(f.complete, "a distinct jump-table switch is recovered");
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Switch(_, arms, _) if arms.len() == 4)),
            "recovers a 4-arm switch",
        );
    }

    #[test]
    fn a_fallthrough_jump_table_recovers() {
        // Fall-through (two case values sharing a body) gives the table repeated
        // entries, rendered as empty lead cases (`case 1: case 2: body`).
        let f = recover_c(
            "int f(int a) { switch (a) { case 1: case 2: return 5; case 3: return 6; case 4: return 7; } return 0; }\n",
        );
        assert!(f.complete, "a fall-through jump table is recovered");
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Switch(_, arms, _) if arms.len() == 4)),
            "four case labels (two of them sharing one body)",
        );
    }

    #[test]
    fn a_switch_default_with_break_recovers() {
        // A `default:` body ending in `break` becomes a real default arm (the
        // third `Switch` field), not post-switch code.
        let f = recover_stack(
            "int f(int a) { int r; r = 0; switch (a) { case 1: r = 1; break; case 2: r = 2; break; default: r = 9; break; } return r; }\n",
        );
        assert!(f.complete);
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Switch(_, arms, def) if arms.len() == 2 && !def.is_empty())),
            "two cases and a non-empty default arm",
        );
    }

    #[test]
    fn an_out_of_order_jump_table_marks_the_function_incomplete() {
        // Source cases out of value order lay the bodies out non-monotonically;
        // the table reader declines rather than mis-attribute them.
        let f = recover_c(
            "int f(int a) { switch (a) { case 4: return 4; case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }\n",
        );
        assert!(!f.complete, "an out-of-order jump table leaves the function incomplete");
    }

    #[test]
    fn a_char_jump_table_recovers_and_types_the_scrutinee_char() {
        // A `char` scrutinee is loaded as a byte and sign-extended (`mov al,[c];
        // cbw; dec ax; mov bx,ax`) before the dispatch. The recovered switch
        // must declare the parameter `char` so the byte-load prologue regenerates.
        let f = recover_c(
            "int f(char c) { switch (c) { case 1: return 1; case 2: return 2; case 3: return 3; case 4: return 4; } return 0; }\n",
        );
        assert!(f.complete, "a char jump-table switch is recovered");
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Switch(_, arms, _) if arms.len() == 4)),
            "recovers a 4-arm switch",
        );
        assert!(f.char_vars.contains(&Var::Param(4)), "the scrutinee param is typed `char`");
        assert!(!f.unsigned_vars.contains(&Var::Param(4)), "a `cbw` scrutinee is signed");
    }

    #[test]
    fn an_unsigned_char_jump_table_types_the_scrutinee_unsigned() {
        // An `unsigned char` scrutinee zero-extends (`mov ah,0`) instead of `cbw`,
        // so it is recovered as both `char` and `unsigned`.
        let f = recover_c(
            "int f(unsigned char c) { switch (c) { case 1: return 1; case 2: return 2; case 3: return 3; case 4: return 4; } return 0; }\n",
        );
        assert!(f.complete, "an unsigned-char jump-table switch is recovered");
        assert!(f.char_vars.contains(&Var::Param(4)), "the scrutinee param is `char`");
        assert!(f.unsigned_vars.contains(&Var::Param(4)), "the `mov ah,0` widen marks it unsigned");
    }

    #[test]
    fn pointer_deref_and_address_of_recover() {
        // `&x` is an AddrOf; `*p` is a Deref; the deref'd variable is a pointer.
        let f = recover_stack("int f() { int x; int *p; x = 3; p = &x; return *p; }\n");
        assert!(f.complete, "address-of and deref are recovered");
        // p (the deref'd variable) is a pointer; x is not.
        assert_eq!(f.ptr_vars.len(), 1, "exactly one pointer (p)");
        // `p = &x`
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Assign(_, Expr::AddrOf(_)))),
            "an `&x` address-of assignment",
        );
        // `return *p`
        assert!(
            matches!(f.body.last(), Some(Stmt::Return(Some(Expr::Deref(_))))),
            "returns a dereference",
        );
    }

    #[test]
    fn long_constant_and_parameter_recover() {
        // `long f() { return 5; }` — `xor dx,dx; mov ax,5` → a long return.
        let f = recover_stack("long f() { return 5; }\n");
        assert!(f.complete);
        assert_eq!(f.ret, Type::Long, "the dx:ax setup marks the return long");
        assert_eq!(f.body, vec![Stmt::Return(Some(Expr::Const(5)))]);

        // A long parameter loaded as a dx:ax pair is recovered and typed long.
        let g = recover_stack("long f(long a) { return a; }\n");
        assert!(g.complete);
        assert_eq!(g.ret, Type::Long);
        assert!(g.long_vars.contains(&Var::Param(4)), "the long param is `long`");
    }

    #[test]
    fn a_long_local_constant_assignment_recovers() {
        // The two-store `long` local (`x = 7;` → store high word, then low) folds
        // to a single `long` assignment. A genuine `long` slot is disambiguated
        // from two adjacent `int` slots by its `dx:ax` read-back, so two `int`
        // stores at offsets differing by 2 are *not* mistaken for one `long`.
        let f = recover_stack("long f() { long x; x = 7; return x; }\n");
        assert!(f.complete, "a long-local constant store pair is recovered");
        assert_eq!(f.long_vars.len(), 1, "exactly one long local");
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Assign(_, Expr::LongConst(7)))),
            "the store pair folds to `x = 7L`",
        );

        // Two adjacent `int` locals (byte-identical store shape) stay two ints.
        let g = recover_stack("int f() { int x; int y; x = 3; y = 4; return x + y; }\n");
        assert!(g.complete && g.long_vars.is_empty(), "two int stores are not a long");
    }

    #[test]
    fn pointer_write_recovers_as_a_deref_assignment() {
        // `*p = v` is `mov bx,p; mov ax,v; mov [bx],ax` → an assignment to `*p`.
        let f = recover_stack("void f(int *p, int v) { *p = v; }\n");
        assert!(f.complete, "a pointer write is recovered");
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Assign(LValue::Deref(_), _))),
            "an assignment through a dereference",
        );
        assert!(f.ptr_vars.contains(&Var::Param(4)), "the written-through pointer is a pointer");
    }

    #[test]
    fn early_return_inside_a_branch_recovers() {
        // `if (a > 0) { return a; } return 0;` — each `return` is `mov ax,val;
        // jmp epilogue`, so the then-block's jump-to-epilogue is a return (not an
        // if/else skip), recovered as a plain `if` whose body returns early.
        let f = recover_stack("int f(int a) { if (a > 0) { return a; } return 0; }\n");
        assert!(f.complete, "an early return inside a branch is recovered");
        let Some(Stmt::If(_, then, els)) = f.body.iter().find(|s| matches!(s, Stmt::If(..))) else {
            panic!("expected an if");
        };
        assert!(els.is_empty(), "the early return is a plain if, not an if/else");
        assert!(matches!(then.last(), Some(Stmt::Return(Some(_)))), "then-block returns");
        assert!(matches!(f.body.last(), Some(Stmt::Return(Some(Expr::Const(0))))), "trailing return 0");
    }
}
