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

use crate::lo_ir::{lift, BinOp, ByteReg, Cond, LoInsn, LoOp, Place, Reg, UnOp};

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
    /// `!e` — logical negation, recovered from the `or r,r; jnz` truthiness test.
    Not(Box<Expr>),
    /// `*e` — a pointer dereference (`mov bx,p; mov ax,[bx]`).
    Deref(Box<Expr>),
    /// `&v` — the address of a variable (`lea ax,[bp+disp]`).
    AddrOf(Var),
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
        Expr::Not(a) | Expr::Deref(a) => contains_call(a),
        Expr::Const(_) | Expr::Var(_) | Expr::AddrOf(_) => false,
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
    /// `return expr;` (or `return;` when the accumulator holds no value).
    Return(Option<Expr>),
    /// `expr;` — an expression evaluated for its side effect (a discarded call).
    ExprStmt(Expr),
    /// `if (cond) { then } else { otherwise }` — `otherwise` empty if no `else`.
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    /// `while (cond) { body }`.
    While(Expr, Vec<Stmt>),
    /// `for (init; cond; step) { body }` — recovered from a `while` whose loop
    /// variable is initialized just before it and stepped at the body's tail
    /// (BCC lowers `for` to exactly that shape).
    For(Box<Stmt>, Expr, Box<Stmt>, Vec<Stmt>),
}

/// Does `expr` mention variable `var`? Used to confirm a `for` loop variable.
fn expr_mentions(expr: &Expr, var: Var) -> bool {
    match expr {
        Expr::Var(v) | Expr::AddrOf(v) => *v == var,
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => {
            expr_mentions(a, var) || expr_mentions(b, var)
        }
        Expr::Not(a) | Expr::Deref(a) => expr_mentions(a, var),
        Expr::Call(args) => args.iter().any(|a| expr_mentions(a, var)),
        Expr::Const(_) => false,
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
        Stmt::For(i, c, st, b) => Stmt::For(i, c, st, fold_for_loops(b)),
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
        let init_var = match out.last() {
            Some(Stmt::Assign(LValue::Var(v), _)) => Some(*v),
            _ => None,
        };
        let step_var = match body.last() {
            Some(Stmt::Assign(LValue::Var(v), _)) => Some(*v),
            _ => None,
        };
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

/// A recovered value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Int,
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
    /// The subset of `vars` that are 32-bit `long` (loaded as a `dx:ax` pair).
    pub long_vars: Vec<Var>,
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
    ptr_vars: Vec<Var>,
    long_vars: Vec<Var>,
    complete: bool,
    returns_value: bool,
    returns_long: bool,
}

impl Ctx {
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

    /// Note a variable that's dereferenced — it's a pointer (`int *`).
    fn note_ptr(&mut self, var: Var) {
        if !self.ptr_vars.contains(&var) {
            self.ptr_vars.push(var);
        }
    }

    /// Note a variable loaded as a `dx:ax` pair — it's a 32-bit `long`.
    fn note_long(&mut self, var: Var) {
        if !self.long_vars.contains(&var) {
            self.long_vars.push(var);
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
        let mut acc: Option<Expr> = None;
        // The pointer value loaded into `bx` (for a `mov bx,p; mov ax,[bx]`
        // dereference).
        let mut bx: Option<Expr> = None;
        // The high word of a `long` being assembled in `dx`, and whether the
        // current accumulator value is a `long` (its high word in `dx`).
        let mut dx: Option<DxState> = None;
        let mut acc_long = false;
        // After an `idiv`/`div`, `dx` holds the remainder of `(dividend, divisor)`
        // — a `mov ax,dx` that reads it recovers the `%` operator.
        let mut dx_rem: Option<(Expr, Expr)> = None;
        // Arguments pushed for the call currently being assembled (push order).
        let mut pending_args: Vec<Expr> = Vec::new();
        // Each `return <expr>` is `mov ax,val; jmp epilogue`, so a jump to the
        // epilogue flushes the accumulator as a `Return`. Tracks whether the run
        // already returned that way, so the physical `Ret` it lands on isn't
        // double-counted (and a void fall-off — no such jump — returns nothing).
        let mut returned = false;
        for i in lo..hi {
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
                | LoOp::Promote { kind: crate::lo_ir::Promote::Cbw | crate::lo_ir::Promote::Cwd } => {}

                // A jump to the epilogue is a `return <accumulator>`.
                LoOp::Jump { target } if self.is_epilogue(target) => {
                    let v = acc.take();
                    if v.is_some() {
                        self.returns_value = true;
                        if acc_long {
                            self.returns_long = true;
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
                    if !returned {
                        // Fell off the end (a void function) — any call left in
                        // the accumulator was a discarded statement, no value.
                        flush_call(&mut acc, out);
                        out.push(Stmt::Return(None));
                    }
                    // Otherwise the `return`s were emitted at the jumps to the
                    // epilogue; the physical `ret` they land on adds nothing.
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
                        None => self.complete = false,
                    }
                }

                // `mov bx, …` loads a pointer for a following `[bx]` dereference.
                LoOp::Load { dst: Place::Reg(Reg::Bx), src } => {
                    flush_call(&mut acc, out);
                    bx = self.operand(src);
                    if bx.is_none() {
                        self.complete = false;
                    }
                }

                // `lea ax, [bp+disp]` — the address of a variable (`&x`).
                LoOp::Lea { dst: Place::Reg(Reg::Ax), src } => {
                    flush_call(&mut acc, out);
                    match Self::var_of(src) {
                        Some(v) => {
                            self.note(v);
                            acc = Some(Expr::AddrOf(v));
                        }
                        None => self.complete = false,
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
                                self.complete = false;
                                acc = None;
                            }
                        }
                        (_, src) => {
                            acc = self.operand(src);
                            if acc.is_none() {
                                self.complete = false;
                            }
                        }
                    }
                }

                // `mov al, …` loads a `char` into the accumulator (a following
                // `cbw` promotes it to `int`, handled above as a no-op).
                LoOp::Load { dst: Place::Byte(ByteReg::Al), src } => {
                    flush_call(&mut acc, out);
                    acc = self.char_operand(src);
                    if acc.is_none() {
                        self.complete = false;
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
                        _ => self.complete = false,
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
                    _ => self.complete = false,
                },

                LoOp::Bin { dst: Place::Reg(Reg::Ax), op, lhs: Place::Reg(Reg::Ax), rhs }
                    if is_foldable(op) =>
                {
                    match (acc.take(), self.operand(rhs)) {
                        (Some(l), Some(r)) => acc = Some(Expr::Binary(op, Box::new(l), Box::new(r))),
                        _ => self.complete = false,
                    }
                }

                // `imul <operand>` — signed multiply; the `int` result is the low
                // word (`ax`). The operand is a memory operand or a constant the
                // multiplier loaded into `dx` (`mov dx,K; imul dx`).
                LoOp::Bin { dst: Place::DxAx, op: BinOp::Imul, lhs: Place::Reg(Reg::Ax), rhs } => {
                    let r = match rhs {
                        Place::Reg(Reg::Dx) => match dx.take() {
                            Some(DxState::Const(v)) => Some(Expr::Const(v)),
                            _ => None,
                        },
                        other => self.operand(other),
                    };
                    match (acc.take(), r) {
                        (Some(l), Some(r)) => {
                            acc = Some(Expr::Binary(BinOp::Imul, Box::new(l), Box::new(r)));
                        }
                        _ => self.complete = false,
                    }
                }

                // `idiv <operand>` — signed divide. The quotient is the low word
                // (`ax`), the remainder the high word (`dx`); a following
                // `mov ax,dx` recovers `%`. The preceding `cwd` set up `dx:ax`
                // from the accumulator (the dividend).
                LoOp::Bin { dst: Place::DxAx, op: BinOp::Idiv, lhs: Place::DxAx, rhs } => {
                    match (acc.take(), self.operand(rhs)) {
                        (Some(l), Some(r)) => {
                            dx_rem = Some((l.clone(), r.clone()));
                            acc = Some(Expr::Binary(BinOp::Idiv, Box::new(l), Box::new(r)));
                        }
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

                // `inc`/`dec ax` (or byte `inc al`) extends the accumulator by
                // ±1 (`x = x + 1` / char `c = c + 1` kept byte-wide).
                LoOp::Un { dst: Place::Reg(Reg::Ax), op, operand: Place::Reg(Reg::Ax) }
                | LoOp::Un { dst: Place::Byte(ByteReg::Al), op, operand: Place::Byte(ByteReg::Al) }
                    if matches!(op, UnOp::Inc | UnOp::Dec) =>
                {
                    let step = if matches!(op, UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match acc.take() {
                        Some(e) => acc = Some(Expr::Binary(step, Box::new(e), Box::new(Expr::Const(1)))),
                        None => self.complete = false,
                    }
                }

                // `inc`/`dec dl` — `c = c ± 1` directly on a char register variable.
                LoOp::Un { dst: Place::Byte(r), op, operand: Place::Byte(o) }
                    if is_byte_reg_var(r) && o == r && matches!(op, UnOp::Inc | UnOp::Dec) =>
                {
                    let step = if matches!(op, UnOp::Inc) { BinOp::Add } else { BinOp::Sub };
                    match self.char_dest(Place::Byte(r)) {
                        Some(lv) => out.push(Stmt::Assign(
                            lv,
                            Expr::Binary(step, Box::new(Expr::Var(Var::ByteReg(r))), Box::new(Expr::Const(1))),
                        )),
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
                        (Some(ptr), Some(e)) => {
                            if let Expr::Var(v) = ptr {
                                self.note_ptr(v);
                            }
                            out.push(Stmt::Assign(LValue::Deref(Box::new(ptr)), e));
                        }
                        _ => self.complete = false,
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
                RelOp::Ne => e,                                          // if (x)
                RelOp::Eq => Expr::Not(Box::new(e)),                     // if (!x)
                RelOp::Lt | RelOp::Le | RelOp::Gt | RelOp::Ge => {
                    Expr::Rel(rel, Box::new(e), Box::new(Expr::Const(0))) // if (x <rel> 0)
                }
            };
        }

        // Otherwise an explicit comparison. An operand may be the accumulator
        // (`cmp ax,n` comparing two memory operands, or `cmp ax,5` on a computed
        // value like `*p`), resolved to the value the run left in the accumulator.
        let LoOp::Bin { dst: Place::Flags, op: BinOp::Cmp, lhs, rhs } = self.insns[cmp_idx].op.clone()
        else {
            return bad(self);
        };
        match (self.cmp_operand(lhs, acc), self.cmp_operand(rhs, acc)) {
            (Some(l), Some(r)) => Expr::Rel(rel, Box::new(l), Box::new(r)),
            _ => bad(self),
        }
    }

    /// A `cmp` operand: a direct operand via [`operand`](Self::operand), or the
    /// accumulator (`ax`/`al`) resolved to the value the test run computed.
    fn cmp_operand(&mut self, place: Place, acc: Option<&Expr>) -> Option<Expr> {
        if matches!(place, Place::Reg(Reg::Ax) | Place::Byte(ByteReg::Al)) {
            acc.cloned()
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
                    // The fold returns the value left in the accumulator (e.g.
                    // `*p` set up before an `or ax,ax` / `cmp ax,K`), which the
                    // condition needs to read a register operand.
                    let cond_acc = self.fold_linear(linear_start, cmp_idx, &mut stmts);
                    let Some(tb) = self.idx_of(target) else {
                        self.complete = false;
                        i += 1;
                        continue;
                    };
                    let cond = self.condition(cmp_idx, i, true, cond_acc.as_ref());
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
    let mut ctx = Ctx {
        insns,
        vars: Vec::new(),
        char_vars: Vec::new(),
        ptr_vars: Vec::new(),
        long_vars: Vec::new(),
        complete: true,
        returns_value: false,
        returns_long: false,
    };
    let len = ctx.insns.len();
    let body = fold_for_loops(ctx.structure(0, len));
    // A `long` occupies two slots (lo, lo+2). If the high-word slot was also
    // recovered as a separate variable (e.g. a `long` local's paired stores read
    // as two `int` stores), the layout is double-counted — bail rather than emit
    // it. (This guards the deferred `long`-local store-pairing case.)
    for &lv in &ctx.long_vars {
        let high = match lv {
            Var::Slot(lo) => Some(Var::Slot(lo + 2)),
            Var::Param(lo) => Some(Var::Param(lo + 2)),
            _ => None,
        };
        if high.is_some_and(|h| ctx.vars.contains(&h)) {
            ctx.complete = false;
        }
    }
    let ret = if ctx.returns_long {
        Type::Long
    } else if ctx.returns_value {
        Type::Int
    } else {
        Type::Void
    };
    Function {
        ret,
        vars: ctx.vars,
        char_vars: ctx.char_vars,
        ptr_vars: ctx.ptr_vars,
        long_vars: ctx.long_vars,
        body,
        complete: ctx.complete,
    }
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
    fn an_unsigned_comparison_marks_the_function_incomplete() {
        // An unsigned compare uses `jb`/`ja`, which `cond_to_relop` doesn't model
        // yet (it needs unsigned operand types) — so the function isn't recovered.
        let f = recover_c("int f(unsigned a) { if (a > 5) { return 1; } return 0; }\n");
        assert!(!f.complete, "an unsigned comparison leaves the function incomplete");
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
    fn a_long_local_assignment_is_incomplete() {
        // The two-store `long` local (`x = 7;` → store high, store low) aliases a
        // two-int layout; the high-word slot doubles as a variable, so the
        // recovery bails rather than emit a double-counted layout.
        let f = recover_stack("long f() { long x; x = 7; return x; }\n");
        assert!(!f.complete, "long-local store pairing is deferred (bails, not wrong)");
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
