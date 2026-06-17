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
        Expr::Const(_) | Expr::LongConst(_) | Expr::Var(_) | Expr::AddrOf(_) => false,
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
    /// `do { body } while (cond);` — a backward branch with no header jump (the
    /// body runs at least once).
    Do(Expr, Vec<Stmt>),
    /// `for (init; cond; step) { body }` — recovered from a `while` whose loop
    /// variable is initialized just before it and stepped at the body's tail
    /// (BCC lowers `for` to exactly that shape).
    For(Box<Stmt>, Expr, Box<Stmt>, Vec<Stmt>),
    /// `switch (scrutinee) { case K: body … }` — recovered from a compare-chain
    /// (`cmp ax,K; je case`)*. No explicit `default`: the no-match path is the
    /// code that follows the switch.
    Switch(Expr, Vec<(i32, Vec<Stmt>)>),
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
        Stmt::Switch(s, arms) => {
            Stmt::Switch(s, arms.into_iter().map(|(k, b)| (k, fold_for_loops(b))).collect())
        }
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

/// The structuring/folding context over one function's Lo-IR.
#[allow(clippy::struct_excessive_bools)] // independent recovery flags, not a state enum
struct Ctx {
    insns: Vec<LoInsn>,
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
            Expr::Not(a) | Expr::Deref(a) => self.mark_unsigned(a),
            Expr::Const(_) | Expr::LongConst(_) | Expr::AddrOf(_) | Expr::Call(_) => {}
        }
    }

    /// Mark the variables directly compared at byte width as `char`.
    fn mark_char(&mut self, expr: &Expr) {
        if let Expr::Var(v) = expr {
            self.note_char(*v);
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
        // Set when an arm consumed the *following* instruction too (a two-store
        // `long` assignment), so the loop skips it.
        let mut skip_next = false;
        for i in lo..hi {
            if skip_next {
                skip_next = false;
                continue;
            }
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

                // `mov dx, ax` — save the accumulator (a binary op's right
                // operand) into `dx` while the left is restored from the stack.
                LoOp::Load { dst: Place::Reg(Reg::Dx), src: Place::Reg(Reg::Ax) } => {
                    dx_temp.clone_from(&acc);
                }

                // `pop ax` — restore the spilled left operand of a binary op.
                LoOp::Pop { dst: Place::Reg(Reg::Ax) } => match pending_args.pop() {
                    Some(e) => acc = Some(e),
                    None => self.complete = false,
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
                        None => self.complete = false,
                    }
                }

                // `mov dx, [bx+2]` — the high word of a `long` read through bx (a
                // `long` array element `a[i]`, or a `long *` deref). Holds the
                // element address until the low half (`mov ax,[bx]`) completes it.
                LoOp::Load { dst: Place::Reg(Reg::Dx), src: Place::DerefDisp(Reg::Bx, 2) } => {
                    long_deref.clone_from(&bx);
                    if long_deref.is_none() {
                        self.complete = false;
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
                        None => self.complete = false,
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
                        _ => self.complete = false,
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
                    None => self.complete = false,
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
                    _ => self.complete = false,
                },

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
                // `cbw` promotes it to `int`, handled above as a no-op). The
                // source is a `char` variable or a byte immediate (`mov al,5`).
                LoOp::Load { dst: Place::Byte(ByteReg::Al), src } => {
                    flush_call(&mut acc, out);
                    acc = match src {
                        Place::Imm(v) => Some(Expr::Const(v)),
                        other => self.char_operand(other),
                    };
                    if acc.is_none() {
                        self.complete = false;
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
                        _ => self.complete = false,
                    }
                }

                // `add ax,lo` / `sub ax,lo` where the accumulator is a `long` is
                // the low-word half; the `adc dx,hi` / `sbb dx,hi` below completes
                // it. Defer until then.
                LoOp::Bin { dst: Place::Reg(Reg::Ax), op, lhs: Place::Reg(Reg::Ax), rhs }
                    if acc_long && matches!(op, BinOp::Add | BinOp::Sub) =>
                {
                    pending_long = Some((op, rhs));
                }

                // `adc dx,hi` / `sbb dx,hi` — the high-word half of a `long`
                // add/sub. The operand is a `long` variable (`[lo]`/`[lo+2]`) or a
                // constant (`(hi<<16)|lo`).
                LoOp::Bin {
                    dst: Place::Reg(Reg::Dx),
                    op: BinOp::Adc | BinOp::Sbb,
                    lhs: Place::Reg(Reg::Dx),
                    rhs: rhs_hi,
                } => match pending_long.take() {
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
                        _ => self.complete = false,
                    },
                    None => self.complete = false,
                },

                LoOp::Bin { dst: Place::Reg(Reg::Ax), op, lhs: Place::Reg(Reg::Ax), rhs }
                    if is_foldable(op) =>
                {
                    match (acc.take(), self.operand(rhs)) {
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
                        _ => self.complete = false,
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
                            self.complete = false;
                        }
                        (Some(ptr), Some(e)) => {
                            if let Expr::Var(v) = ptr {
                                self.note_ptr(v);
                            }
                            out.push(Stmt::Assign(LValue::Deref(Box::new(ptr)), e));
                        }
                        _ => self.complete = false,
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
                            skip_next = true;
                        }
                        None => match self.dest(Place::Local(hi)) {
                            Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(imm_hi))),
                            None => self.complete = false,
                        },
                    }
                }

                LoOp::Store { dst, src: Place::Imm(v) } => {
                    flush_call(&mut acc, out);
                    match self.dest(dst) {
                        Some(lv) => out.push(Stmt::Assign(lv, Expr::Const(v))),
                        None => self.complete = false,
                    }
                }

                // `mov byte ptr [bx], imm` — a `char` immediate stored through a
                // `char *` (`*p = const`).
                LoOp::StoreImmByte { dst: Place::Deref(Reg::Bx), imm } => {
                    flush_call(&mut acc, out);
                    match bx.clone() {
                        Some(ptr) => {
                            if let Expr::Var(v) = ptr {
                                self.note_char_ptr(v);
                            }
                            out.push(Stmt::Assign(LValue::Deref(Box::new(ptr)), Expr::Const(imm)));
                        }
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
                    // Forward conditional branch → an `if` (or a `switch`). The
                    // `cmp` feeding it is the previous instruction; everything
                    // before that is a straight-line run.
                    if i == 0 {
                        self.complete = false;
                        i += 1;
                        continue;
                    }
                    let cmp_idx = i - 1;

                    // A compare-chain (`cmp ax,Ki; je Ti` × N) is a `switch`. The
                    // scrutinee is the value the run left in `ax`; each case body
                    // runs from its target to the next case's (or the no-match
                    // block); the no-match block is the post-switch code.
                    if let Some((cases, def_idx)) = self.detect_switch(cmp_idx) {
                        let scrut = self.fold_linear(linear_start, cmp_idx, &mut stmts);
                        match scrut {
                            Some(scrutinee) => {
                                let mut arms = Vec::new();
                                for (j, &(value, start)) in cases.iter().enumerate() {
                                    let end = cases.get(j + 1).map_or(def_idx, |&(_, s)| s);
                                    arms.push((value, self.structure(start, end)));
                                }
                                stmts.push(Stmt::Switch(scrutinee, arms));
                            }
                            None => self.complete = false,
                        }
                        i = def_idx;
                        linear_start = def_idx;
                        continue;
                    }

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
                        self.complete = false;
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
/// fold it into statements.
#[must_use]
pub fn recover(code: &[u8]) -> Function {
    let insns: Vec<LoInsn> = lift(code);
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
    let mut ctx = Ctx {
        insns,
        vars: Vec::new(),
        char_vars: Vec::new(),
        ptr_vars: Vec::new(),
        char_ptr_vars: Vec::new(),
        long_vars: Vec::new(),
        unsigned_vars: Vec::new(),
        long_slots,
        char_array_bases: Vec::new(),
        long_array_bases: Vec::new(),
        complete: true,
        returns_value: false,
        returns_long: false,
        returns_char: false,
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
    let complete = ctx.complete
        && ctx.char_array_bases.iter().chain(&ctx.long_array_bases).all(|&b| covered(b));
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
    // frame (a `char` frame is padded up to even). Keep them as scalars.
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
            f.body.iter().any(|s| matches!(s, Stmt::Switch(_, arms) if arms.len() == 3)),
            "recovers a 3-arm switch",
        );
    }

    #[test]
    fn a_jump_table_switch_marks_the_function_incomplete() {
        // A dense switch (≥ 4 contiguous cases) BCC lowers to a jump table
        // (`dec bx; cmp bx,N; ja default; jmp cs:[bx+table]`), not yet structured.
        let f = recover_c(
            "int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; case 4: return 4; } return 0; }\n",
        );
        assert!(!f.complete, "a jump-table switch leaves the function incomplete");
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
