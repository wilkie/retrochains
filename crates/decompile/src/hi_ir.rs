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

use crate::lo_ir::{lift, BinOp, Cond, LoInsn, LoOp, Place, Reg, UnOp};

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
    /// A local variable — a stack slot, parameter, or register variable.
    Var(Var),
    /// `lhs op rhs` (left-associative, as the accumulator chain produces).
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `lhs rel rhs` — a comparison (the condition of an `if`/`while`).
    Rel(RelOp, Box<Expr>, Box<Expr>),
    /// A call with its argument list (source order). The callee is an opaque
    /// external: a `call` is a placeholder `e8 00 00` patched by a relocation,
    /// so the target's identity isn't in `_TEXT` and any declared extern
    /// reproduces the bytes — only the argument count/types matter.
    Call(Vec<Expr>),
}

/// Does an expression contain a (side-effecting) call anywhere?
fn contains_call(e: &Expr) -> bool {
    match e {
        Expr::Call(_) => true,
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => contains_call(a) || contains_call(b),
        Expr::Const(_) | Expr::Var(_) => false,
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
}

/// An assignable location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LValue {
    /// A local variable.
    Var(Var),
}

/// Is `r` one of the registers BCC uses for register variables?
fn is_reg_var(r: Reg) -> bool {
    matches!(r, Reg::Si | Reg::Di)
}

/// A recovered statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `lvalue = expr;`
    Assign(LValue, Expr),
    /// `return expr;` (or `return;` when the accumulator holds no value).
    Return(Option<Expr>),
    /// `expr;` — an expression evaluated for its side effect (a discarded call).
    ExprStmt(Expr),
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
    /// The local variables the body touches, in first-appearance order.
    pub vars: Vec<Var>,
    /// The subset of `vars` accessed at byte width — these are `char` (the rest
    /// are `int`). Width is inferred from the access, not guessed.
    pub char_vars: Vec<Var>,
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

/// The structuring/folding context over one function's Lo-IR.
struct Ctx {
    insns: Vec<LoInsn>,
    vars: Vec<Var>,
    char_vars: Vec<Var>,
    complete: bool,
    returns_value: bool,
}

impl Ctx {
    /// Record a variable the first time it's seen (preserving order).
    fn note(&mut self, var: Var) {
        if !self.vars.contains(&var) {
            self.vars.push(var);
        }
    }

    /// Note a variable accessed at byte width — it's a `char`.
    fn note_char(&mut self, var: Var) {
        if !self.char_vars.contains(&var) {
            self.char_vars.push(var);
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
            _ => None,
        }
    }

    /// The instruction index whose byte offset is `off` (first match), if any.
    fn idx_of(&self, off: usize) -> Option<usize> {
        self.insns.iter().position(|i| i.span.start == off)
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
    #[allow(clippy::too_many_lines)] // a flat per-op match reads better unsplit
    fn fold_linear(&mut self, lo: usize, hi: usize, out: &mut Vec<Stmt>) {
        let mut acc: Option<Expr> = None;
        // Arguments pushed for the call currently being assembled (push order).
        let mut pending_args: Vec<Expr> = Vec::new();
        // BCC emits its redundant exit jump (`eb 00`) for an explicit `return`
        // but not when a void function falls off the end — so a `Jump` before
        // the epilogue distinguishes "return the accumulator" from "the trailing
        // value is a discarded call".
        let mut saw_exit_jump = false;
        let mut emitted_ret = false;
        for i in lo..hi {
            match self.insns[i].op.clone() {
                // Frame setup/teardown carry no value, and `cbw` is the implicit
                // `char`→`int` promotion — the accumulator already holds the
                // value, and emitting the source recompiles to the same `cbw`.
                LoOp::Enter { .. }
                | LoOp::Leave
                | LoOp::SaveReg { .. }
                | LoOp::RestoreReg { .. }
                | LoOp::Cleanup { .. }
                | LoOp::Promote { kind: crate::lo_ir::Promote::Cbw } => {}

                LoOp::Jump { .. } => saw_exit_jump = true,

                // `push ax` — the accumulator is the argument; `push [bp+d]` or a
                // register push supplies a variable directly.
                LoOp::Arg { src: Place::Reg(Reg::Ax) } => match acc.take() {
                    Some(e) => pending_args.push(e),
                    None => self.complete = false,
                },
                LoOp::Arg { src } => {
                    flush_call(&mut acc, out);
                    match self.operand(src) {
                        Some(e) => pending_args.push(e),
                        None => self.complete = false,
                    }
                }

                // A call consumes the pending arguments (cdecl pushes them
                // right-to-left, so reverse to source order) and leaves its
                // result in the accumulator.
                LoOp::Call { .. } => {
                    let mut args = std::mem::take(&mut pending_args);
                    args.reverse();
                    acc = Some(Expr::Call(args));
                }

                LoOp::Ret { .. } => {
                    emitted_ret = true;
                    if saw_exit_jump {
                        // An explicit `return <expr>` — the accumulator is the value.
                        let v = acc.take();
                        if v.is_some() {
                            self.returns_value = true;
                        }
                        out.push(Stmt::Return(v));
                    } else {
                        // Fell off the end — any call in the accumulator was a
                        // discarded statement, and there's no return value.
                        flush_call(&mut acc, out);
                        out.push(Stmt::Return(None));
                    }
                }

                // `mov ax, …` starts a fresh accumulator value, discarding any
                // call result the previous statement left unused.
                LoOp::Load { dst: Place::Reg(Reg::Ax), src } => {
                    flush_call(&mut acc, out);
                    acc = self.operand(src);
                    if acc.is_none() {
                        self.complete = false;
                    }
                }

                // `mov al, …` loads a `char` into the accumulator (a following
                // `cbw` promotes it to `int`, handled below as a no-op).
                LoOp::Load { dst: Place::Byte(_), src } => {
                    flush_call(&mut acc, out);
                    acc = self.char_operand(src);
                    if acc.is_none() {
                        self.complete = false;
                    }
                }

                // `mov si, …` — an assignment to a register variable. The source
                // is an immediate, the accumulator (`mov si,ax` stores the
                // current expression), or another register variable.
                LoOp::Load { dst: Place::Reg(r), src } if is_reg_var(r) => {
                    let val = match src {
                        Place::Reg(Reg::Ax) => acc.take(), // consumes the accumulator
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
                        _ => self.complete = false,
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

                // `xor si,si` — the zero idiom on a register variable (`x = 0`).
                LoOp::Bin { dst: Place::Reg(r), op: BinOp::Xor, lhs: Place::Reg(a), rhs: Place::Reg(b) }
                    if is_reg_var(r) && a == r && b == r =>
                {
                    match self.dest(Place::Reg(r)) {
                        Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(0))),
                        None => self.complete = false,
                    }
                }

                // `inc`/`dec ax` extends the accumulator by ±1 (`x = x + 1`).
                LoOp::Un { dst: Place::Reg(Reg::Ax), op, operand: Place::Reg(Reg::Ax) }
                    if matches!(op, UnOp::Inc | UnOp::Dec) =>
                {
                    let step = if matches!(op, UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match acc.take() {
                        Some(e) => acc = Some(Expr::Binary(step, Box::new(e), Box::new(Expr::Const(1)))),
                        None => self.complete = false,
                    }
                }

                // `inc`/`dec si` — `x = x ± 1` directly on a register variable.
                LoOp::Un { dst: Place::Reg(r), op, operand: Place::Reg(o) }
                    if is_reg_var(r) && o == r && matches!(op, UnOp::Inc | UnOp::Dec) =>
                {
                    let step = if matches!(op, UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match self.dest(Place::Reg(r)) {
                        Some(lv) => out.push(Stmt::Assign(
                            lv,
                            Expr::Binary(step, Box::new(Expr::Var(Var::Reg(r))), Box::new(Expr::Const(1))),
                        )),
                        None => self.complete = false,
                    }
                }

                LoOp::Store { dst, src: Place::Reg(Reg::Ax) } => {
                    match (self.dest(dst), acc.take()) {
                        (Some(lv), Some(e)) => out.push(Stmt::Assign(lv, e)),
                        _ => self.complete = false,
                    }
                }

                // `mov [dst], al` — store the accumulator to a `char`.
                LoOp::Store { dst, src: Place::Byte(_) } => {
                    match (self.char_dest(dst), acc.take()) {
                        (Some(lv), Some(e)) => out.push(Stmt::Assign(lv, e)),
                        _ => self.complete = false,
                    }
                }

                LoOp::Store { dst, src: Place::Imm(v) } => {
                    flush_call(&mut acc, out);
                    match self.dest(dst) {
                        Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(v))),
                        None => self.complete = false,
                    }
                }

                // `mov byte ptr [dst], imm` — a `char` immediate store.
                LoOp::StoreImmByte { dst, imm } => {
                    flush_call(&mut acc, out);
                    match self.char_dest(dst) {
                        Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(imm))),
                        None => self.complete = false,
                    }
                }

                _ => self.complete = false,
            }
        }
        // A discarded call at the run's end (a trailing `g(x);`) still happened.
        flush_call(&mut acc, out);
        if !pending_args.is_empty() {
            self.complete = false; // args pushed with no call to consume them
        }
        // A jump to the epilogue with no `return` in this run is an early return
        // inside a branch/loop — a multi-exit shape this increment doesn't
        // structure. Bail rather than silently drop the returned value.
        if saw_exit_jump && !emitted_ret {
            self.complete = false;
        }
    }

    /// Recover the condition of an `if`/`while` from the test at `cmp_idx` and
    /// the branch at `branch_idx`. `negate` inverts it (for a skip branch).
    ///
    /// Two test shapes feed a branch: an explicit `cmp lhs, rhs`, and the
    /// register-variable truthiness idiom `or si,si` (which sets flags without
    /// changing `si`) — the latter is a comparison of the variable against 0.
    fn condition(&mut self, cmp_idx: usize, branch_idx: usize, negate: bool) -> Expr {
        let bad = |s: &mut Self| {
            s.complete = false;
            Expr::Const(0)
        };
        let (lhs, rhs) = match self.insns[cmp_idx].op.clone() {
            LoOp::Bin { dst: Place::Flags, op: BinOp::Cmp, lhs, rhs } => (lhs, rhs),
            // `or si,si` — test a register variable against 0.
            LoOp::Bin { dst: Place::Reg(r), op: BinOp::Or, lhs: Place::Reg(a), rhs: Place::Reg(b) }
                if is_reg_var(r) && a == r && b == r =>
            {
                (Place::Reg(r), Place::Imm(0))
            }
            _ => return bad(self),
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
    let mut ctx =
        Ctx { insns, vars: Vec::new(), char_vars: Vec::new(), complete: true, returns_value: false };
    let len = ctx.insns.len();
    let body = ctx.structure(0, len);
    let ret = if ctx.returns_value { Type::Int } else { Type::Void };
    Function { ret, vars: ctx.vars, char_vars: ctx.char_vars, body, complete: ctx.complete }
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
        // idiom must recover as `x != 0`.
        let f = recover_c("int f() { int x; x = 0; if (x) { x = 1; } return x; }\n");
        assert!(f.complete, "register-variable if is recovered");
        let Stmt::If(Expr::Rel(op, lhs, rhs), ..) =
            f.body.iter().find(|s| matches!(s, Stmt::If(..))).expect("an if")
        else {
            panic!("expected a relational condition");
        };
        assert_eq!(*op, RelOp::Ne, "or si,si; jz → x != 0");
        assert!(matches!(**lhs, Expr::Var(Var::Reg(Reg::Si))));
        assert_eq!(**rhs, Expr::Const(0));
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
    fn call_recovers_with_arguments_and_a_parameter() {
        // `g(a)` — a parameter passed to a call. The parameter is `[bp+4]`, the
        // call result is returned.
        let f = recover_c("extern int g(); int f(int a) { return g(a); }\n");
        assert!(f.complete, "a call with a parameter argument is recovered");
        assert!(f.vars.contains(&Var::Param(4)), "the parameter is [bp+4]");
        let Some(Stmt::Return(Some(Expr::Call(args)))) = f.body.last() else {
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
            matches!(f.body.first(), Some(Stmt::ExprStmt(Expr::Call(_)))),
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
    fn a_multiply_marks_the_function_incomplete() {
        // `a * a` lowers to imul (dx:ax), which the int-accumulator fold doesn't
        // model — so the function isn't recovered.
        let f = recover_c("int f(int a) { return a * a; }\n");
        assert!(!f.complete, "an unmodelled multiply leaves the function incomplete");
    }

    #[test]
    fn an_early_return_marks_the_function_incomplete() {
        // `if (a > 0) { return a; }` is a multi-exit shape this increment doesn't
        // structure — it must bail rather than silently drop the early return.
        let f = recover_c("int f(int a) { if (a > 0) { return a; } return 0; }\n");
        assert!(!f.complete, "an early return inside a branch leaves the function incomplete");
    }
}
