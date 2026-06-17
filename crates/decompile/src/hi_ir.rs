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

use crate::lo_ir::{lift, BinOp, Cond, LoInsn, LoOp, Place, Reg};

/// A relational operator recovered from a `cmp` + conditional branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
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
        }
    }
}

/// The relational operator a `Jcc` low-nibble condition tests, or `None` for the
/// codes this increment doesn't model (unsigned and the flag-only jumps). The
/// branch is *taken* when this holds.
fn cond_to_relop(cond: Cond) -> Option<RelOp> {
    match cond.0 {
        0x4 => Some(RelOp::Eq), // jz / je
        0x5 => Some(RelOp::Ne), // jnz / jne
        0xc => Some(RelOp::Lt), // jl
        0xd => Some(RelOp::Ge), // jge
        0xe => Some(RelOp::Le), // jle
        0xf => Some(RelOp::Gt), // jg
        _ => None,              // unsigned (jb/ja/…) and jo/js/jp need more type info
    }
}

/// A recovered expression — a subset of the §5 grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// An integer constant.
    Const(i32),
    /// A local variable, identified by its `[bp+disp]` slot (`disp < 0`).
    Local(i16),
    /// `lhs op rhs` (left-associative, as the accumulator chain produces).
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `lhs rel rhs` — a comparison (the condition of an `if`/`while`).
    Rel(RelOp, Box<Expr>, Box<Expr>),
}

/// An assignable location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LValue {
    /// A local variable slot.
    Local(i16),
}

/// A recovered statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `lvalue = expr;`
    Assign(LValue, Expr),
    /// `return expr;` (or `return;` when the accumulator holds no value).
    Return(Option<Expr>),
    /// `if (cond) { then } else { otherwise }` — `otherwise` empty if no `else`.
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    /// `while (cond) { body }`.
    While(Expr, Vec<Stmt>),
}

/// A recovered value type. Minimal for now — this increment is all `int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Int,
    Void,
}

/// A recovered function body. The signature is intentionally thin: recompilation
/// doesn't depend on the function's *name* (it isn't in `_TEXT`), so we emit a
/// placeholder name and focus on the body and the slots it touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// The return type, inferred from whether any `return` carries a value.
    pub ret: Type,
    /// The local slots the body touches, in first-appearance order. All `int`.
    pub locals: Vec<i16>,
    /// The recovered statements.
    pub body: Vec<Stmt>,
    /// `false` if any op couldn't be modelled — the function is only partially
    /// recovered and must not be presented as done.
    pub complete: bool,
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

/// The structuring/folding context over one function's Lo-IR.
struct Ctx {
    insns: Vec<LoInsn>,
    locals: Vec<i16>,
    complete: bool,
    returns_value: bool,
}

impl Ctx {
    /// Record a local slot the first time it's seen (preserving order).
    fn note(&mut self, slot: i16) {
        if !self.locals.contains(&slot) {
            self.locals.push(slot);
        }
    }

    /// The instruction index whose byte offset is `off` (first match), if any.
    fn idx_of(&self, off: usize) -> Option<usize> {
        self.insns.iter().position(|i| i.span.start == off)
    }

    /// A source operand as an [`Expr`], or `None` if outside this increment (a
    /// param, global, register, pointer, …).
    fn operand(&mut self, place: Place) -> Option<Expr> {
        match place {
            Place::Imm(v) => Some(Expr::Const(v)),
            Place::Local(d) if d < 0 => {
                self.note(d);
                Some(Expr::Local(d))
            }
            _ => None,
        }
    }

    /// A destination operand as an [`LValue`], or `None` if unsupported yet.
    fn dest(&mut self, place: Place) -> Option<LValue> {
        match place {
            Place::Local(d) if d < 0 => {
                self.note(d);
                Some(LValue::Local(d))
            }
            _ => None,
        }
    }

    /// Fold a straight-line instruction range `[lo, hi)` into statements,
    /// symbolically tracking the accumulator. Any op it can't model marks the
    /// function incomplete.
    fn fold_linear(&mut self, lo: usize, hi: usize, out: &mut Vec<Stmt>) {
        let mut acc: Option<Expr> = None;
        for i in lo..hi {
            match self.insns[i].op.clone() {
                // Frame setup/teardown and unconditional jumps carry no value
                // (a straight-line run's only jump is the stereotyped exit).
                LoOp::Enter { .. }
                | LoOp::Leave
                | LoOp::Jump { .. }
                | LoOp::SaveReg { .. }
                | LoOp::RestoreReg { .. } => {}

                LoOp::Ret { .. } => {
                    let v = acc.take();
                    if v.is_some() {
                        self.returns_value = true;
                    }
                    out.push(Stmt::Return(v));
                }

                LoOp::Load { dst: Place::Reg(Reg::Ax), src } => {
                    acc = self.operand(src);
                    if acc.is_none() {
                        self.complete = false;
                    }
                }

                LoOp::Bin { dst: Place::Reg(Reg::Ax), op, lhs: Place::Reg(Reg::Ax), rhs }
                    if is_foldable(op) =>
                {
                    match (acc.take(), self.operand(rhs)) {
                        (Some(l), Some(r)) => acc = Some(Expr::Binary(op, Box::new(l), Box::new(r))),
                        _ => self.complete = false,
                    }
                }

                // `inc`/`dec ax` extends the accumulator by ±1 (`x = x + 1`).
                LoOp::Un { dst: Place::Reg(Reg::Ax), op, operand: Place::Reg(Reg::Ax) }
                    if matches!(op, crate::lo_ir::UnOp::Inc | crate::lo_ir::UnOp::Dec) =>
                {
                    let step = if matches!(op, crate::lo_ir::UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match acc.take() {
                        Some(e) => acc = Some(Expr::Binary(step, Box::new(e), Box::new(Expr::Const(1)))),
                        None => self.complete = false,
                    }
                }

                LoOp::Store { dst, src: Place::Reg(Reg::Ax) } => {
                    match (self.dest(dst), acc.take()) {
                        (Some(lv), Some(e)) => out.push(Stmt::Assign(lv, e)),
                        _ => self.complete = false,
                    }
                }

                LoOp::Store { dst, src: Place::Imm(v) } => match self.dest(dst) {
                    Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(v))),
                    None => self.complete = false,
                },

                _ => self.complete = false,
            }
        }
    }

    /// Recover the condition of an `if`/`while` from the `cmp` at `cmp_idx` and
    /// the branch at `branch_idx`. `negate` inverts it (for a skip branch).
    fn condition(&mut self, cmp_idx: usize, branch_idx: usize, negate: bool) -> Expr {
        let bad = |s: &mut Self| {
            s.complete = false;
            Expr::Const(0)
        };
        let LoOp::Bin { dst: Place::Flags, op: BinOp::Cmp, lhs, rhs } = self.insns[cmp_idx].op.clone()
        else {
            return bad(self);
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
        match (self.operand(lhs), self.operand(rhs)) {
            (Some(l), Some(r)) => Expr::Rel(rel, Box::new(l), Box::new(r)),
            _ => bad(self),
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
        if e <= then_target {
            return None;
        }
        self.idx_of(e)
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
                // The test must be exactly the `cmp` the branch reads: the header
                // jump lands on it and it sits just before the branch
                // (single-compare condition — the common shape).
                if k > 0 && test_idx == k - 1 {
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
                    // Forward conditional branch → an `if`. The `cmp` feeding it
                    // is the previous instruction; everything before that is a
                    // straight-line run.
                    if i == 0 {
                        self.complete = false;
                        i += 1;
                        continue;
                    }
                    let cmp_idx = i - 1;
                    self.fold_linear(linear_start, cmp_idx, &mut stmts);
                    let Some(tb) = self.idx_of(target) else {
                        self.complete = false;
                        i += 1;
                        continue;
                    };
                    let cond = self.condition(cmp_idx, i, true);
                    // `if/else` when the then-block ends in a jump past the
                    // else-block; otherwise a plain `if`.
                    let resume = if let Some(e_idx) = self.else_end(tb, target) {
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
                        let cond = self.condition(cmp_idx, br_idx, false);
                        let body = self.structure(b_lo, b_hi);
                        stmts.push(Stmt::While(cond, body));
                        i = cont;
                        linear_start = cont;
                    } else {
                        i += 1; // exit jump — folded (ignored) with the linear run
                    }
                }
                Some((true, _)) => {
                    // A backward conditional branch we didn't fold into a loop.
                    self.complete = false;
                    i += 1;
                }
                None => i += 1,
            }
        }
        self.fold_linear(linear_start, hi, &mut stmts);
        stmts
    }
}

/// Recover a function body from `_TEXT` bytes: lift to Lo-IR, then structure and
/// fold it into statements.
#[must_use]
pub fn recover(code: &[u8]) -> Function {
    let insns: Vec<LoInsn> = lift(code);
    let mut ctx = Ctx { insns, locals: Vec::new(), complete: true, returns_value: false };
    let len = ctx.insns.len();
    let body = ctx.structure(0, len);
    let ret = if ctx.returns_value { Type::Int } else { Type::Void };
    Function { ret, locals: ctx.locals, body, complete: ctx.complete }
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
    fn assign_then_return_folds_through_the_slot() {
        let f = recover_c("int f() { int x; x = 5; return x; }\n");
        assert!(f.complete, "straight-line int code is fully recovered");
        assert_eq!(f.locals.len(), 1, "one local slot");
        let slot = f.locals[0];
        assert_eq!(
            f.body,
            vec![
                Stmt::Assign(LValue::Local(slot), Expr::Const(5)),
                Stmt::Return(Some(Expr::Local(slot))),
            ],
        );
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
    fn a_call_marks_the_function_incomplete() {
        let f = recover_c("int g(); int f() { return g(); }\n");
        assert!(!f.complete, "an unfolded call leaves the function incomplete");
    }
}
