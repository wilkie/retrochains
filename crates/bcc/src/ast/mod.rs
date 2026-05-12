//! AST types. Faithful to source order; every node carries the byte span
//! it covers so codegen can find the original source line(s) that backed
//! it (BCC interleaves source lines as `;` comments — we need the
//! original bytes to reproduce that).

use crate::lex::Span;

/// A whole translation unit (one `.C` file).
#[derive(Debug)]
pub struct Unit {
    pub functions: Vec<Function>,
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    /// Parameters in source order. The empty list represents `(void)`
    /// — C's "no parameters" spelling — not a variadic prototype.
    pub params: Vec<Param>,
    /// The full byte range of the function definition, from the
    /// `int main…` opening through the closing `}`. Used to compute
    /// which source lines this function spans, which in turn drives the
    /// `;` source-comment emission in the asm.
    pub span: Span,
    pub body: Vec<Stmt>,
}

#[derive(Debug)]
pub struct Param {
    pub name: String,
    pub ty: Type,
}

/// One arm of a `switch`. `value: None` represents the `default:`
/// arm. The span starts at the `case` / `default` keyword and ends
/// one past the colon, so source-line tracking can emit the comment
/// block tied to the arm's header line.
#[derive(Debug)]
pub struct SwitchCase {
    pub value: Option<u32>,
    pub span: Span,
    pub body: Vec<Stmt>,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    Return(Option<Expr>),
    /// `<type> <name> [= <init>];`. For now only `int` and a single
    /// declarator with an optional initializer.
    Declare { ty: Type, name: String, init: Option<Expr> },
    /// `if (cond) then-body [else else-body]`.
    If { cond: Expr, then_branch: Vec<Stmt>, else_branch: Option<Vec<Stmt>> },
    /// `<name> = <value>;`. The bare-ident assignment form. Pointer-
    /// indirect (`*p = v;`) and array-index (`a[i] = v;`) targets get
    /// their own statement kinds because the asm shape is distinct
    /// enough that lumping them into one node would just split into a
    /// match at codegen anyway.
    Assign { name: String, value: Expr },
    /// `a[<index>] = <value>;`. `array` is the base name of a local
    /// of array type (no pointer arithmetic yet — that would lower to
    /// `DerefAssign` of a computed pointer). Index can be any int
    /// expression; codegen specializes the constant-index case.
    ArrayAssign { array: String, index: Expr, value: Expr },
    /// `*<target> = <value>;`. `target` is the pointer expression
    /// — usually an `Ident` for a pointer local, but in principle
    /// anything that evaluates to a pointer.
    DerefAssign { target: Expr, value: Expr },
    /// `<name> <op>= <value>;` (compound assignment). The codegen
    /// is distinct from `Assign { name, value: name <op> value }` —
    /// BCC emits a tighter form using `<op> <dst>, <src>` directly
    /// rather than the AX round-trip (fixtures 067-071).
    CompoundAssign { name: String, op: BinOp, value: Expr },
    /// `while (cond) body`.
    While { cond: Expr, body: Vec<Stmt> },
    /// `do body while (cond);` — bottom-checking loop, body runs at
    /// least once.
    DoWhile { body: Vec<Stmt>, cond: Expr },
    /// `for (init; cond; step) body`. Any of init/cond/step may be
    /// absent (C lets you omit each clause); we'll model that as
    /// `Option<Expr>` (init/step as expressions only — we don't yet
    /// support C99-style declarations in the init clause).
    For {
        init: Option<Expr>,
        cond: Option<Expr>,
        step: Option<Expr>,
        body: Vec<Stmt>,
    },
    /// `break;` — exit the innermost enclosing loop or switch.
    Break,
    /// `continue;` — jump to the next iteration of the innermost
    /// enclosing loop (i.e., to its check / step label). Switches do
    /// not catch `continue;` — it threads past them to the loop.
    Continue,
    /// `switch (scrutinee) { case K: ...; default: ...; }`. Cases are
    /// kept in source order; at most one may be a `default` (None
    /// value). Each case's body extends until the next `case` /
    /// `default` / closing brace — i.e. fall-through is modeled as
    /// "no `break;` at the end", and codegen emits case bodies
    /// linearly so control just continues into the next case label.
    Switch {
        scrutinee: Expr,
        cases: Vec<SwitchCase>,
    },
    /// Expression evaluated for its side effects, value discarded.
    /// Examples: `++x;`, `f();`. Plain expressions without side
    /// effects (`5;`) are syntactically valid but semantically a no-op
    /// — codegen still emits a (no-op) load to match BCC if a fixture
    /// ever pins it.
    ExprStmt(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    /// `int` — 16-bit signed under the small memory model.
    Int,
    /// `char` — 1-byte signed (BCC's default for plain `char`).
    Char,
    /// `T a[N]` — contiguous run of `N` `T`-typed elements on the stack.
    /// Arrays never enregister; the name in expression contexts refers
    /// to a stack address. Today only constant `N` is supported (we
    /// store the count as a `u32` matching `IntLit`'s width); the
    /// element type is itself a `Type` so multi-dimensional arrays
    /// fall out naturally when a fixture demands them.
    Array { elem: Box<Type>, len: u32 },
    /// `T *p` — a 16-bit near pointer to a `T`. Under the small memory
    /// model all pointers are 2 bytes (intra-segment). Pointer locals
    /// are eligible for the int register pool (SI/DI/DX/BX/CX) and
    /// enregister at a *lower* use threshold than ints (≥ 2 vs. ≥ 3),
    /// pinned by fixtures 080 and 081.
    Pointer(Box<Type>),
}

impl Type {
    /// Size in bytes for stack-frame allocation. Memory-model-specific
    /// details (far pointers, etc.) will come later.
    #[must_use]
    pub fn size_bytes(&self) -> u16 {
        match self {
            Self::Int => 2,
            Self::Char => 1,
            Self::Array { elem, len } => {
                let elem = elem.size_bytes();
                elem * u16::try_from(*len).expect("array byte size fits in u16")
            }
            Self::Pointer(_) => 2,
        }
    }

    /// Alignment (in bytes) required for this type's stack slot. `int`
    /// must land on an even bp-offset (so BCC pads when a preceding
    /// `char` left the cursor at an odd offset — see fixture 011).
    /// Arrays align like their element type; pointers like ints.
    #[must_use]
    pub fn alignment(&self) -> u16 {
        match self {
            Self::Int => 2,
            Self::Char => 1,
            Self::Array { elem, .. } => elem.alignment(),
            Self::Pointer(_) => 2,
        }
    }

    /// Whether this type can sit in an int-pool register (SI/DI/DX/BX/CX).
    /// True for `int` and any pointer. Arrays never enregister.
    #[must_use]
    pub fn is_int_like(&self) -> bool {
        matches!(self, Self::Int | Self::Pointer(_))
    }

    /// The element type of an array, or `None` if not an array. Used
    /// by codegen to pick the right stride / width when indexing.
    #[must_use]
    pub fn array_elem(&self) -> Option<&Type> {
        if let Self::Array { elem, .. } = self {
            Some(elem)
        } else {
            None
        }
    }

    /// The pointee type of a pointer, or `None` if not a pointer.
    /// Used by codegen to pick the deref width.
    #[must_use]
    pub fn pointee(&self) -> Option<&Type> {
        if let Self::Pointer(inner) = self {
            Some(inner)
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind {
    IntLit(u32),
    Ident(String),
    BinOp { op: BinOp, left: Box<Expr>, right: Box<Expr> },
    /// Prefix unary operator: `-e`, `!e`, `~e`.
    Unary { op: UnaryOp, operand: Box<Expr> },
    /// Short-circuiting logical operator: `a && b`, `a || b`. Held
    /// separately from `BinOp` because the right operand may not be
    /// evaluated, and the codegen patterns are completely different
    /// from the regular binary ops.
    Logical { op: LogicalOp, left: Box<Expr>, right: Box<Expr> },
    /// `++name` / `--name` / `name++` / `name--`. Today the target
    /// must be a bare identifier referring to a local or parameter.
    Update { target: String, op: UpdateOp, position: UpdatePosition },
    /// `name = value` as an expression. In C, assignment is an
    /// expression that yields the assigned value; we have it as a
    /// statement (`StmtKind::Assign`) for the common case, and this
    /// expression form covers `for (i = 0; ...; ...)` init/step
    /// clauses where the assignment appears in expression position.
    AssignExpr { target: String, value: Box<Expr> },
    /// Direct function call by name.
    Call { name: String, args: Vec<Expr> },
    /// `&<name>` — address-of a named local or parameter. Restricted
    /// to a bare ident today; the more general `&<lvalue>` (e.g.
    /// `&a[i]`) doesn't appear in fixtures yet.
    AddressOf(String),
    /// `*<ptr>` — pointer dereference in an rvalue context. The
    /// pointee width comes from the static type of `ptr`.
    Deref(Box<Expr>),
    /// `<array>[<index>]` in an rvalue context. `array` is the name
    /// of a local of array type; codegen specializes constant indices
    /// to a plain `[bp-N]` load and falls back to the 5-instruction
    /// effective-address sequence for variable indices.
    ArrayIndex { array: String, index: Box<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    /// `&&`. Short-circuit: false on left → result is 0, right not
    /// evaluated.
    And,
    /// `||`. Short-circuit: true on left → result is 1, right not
    /// evaluated.
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Inc,
    Dec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePosition {
    /// `++x` / `--x` — the *new* value is the expression's value.
    Pre,
    /// `x++` / `x--` — the *old* value is the expression's value.
    Post,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation. Emits `neg ax` at runtime; folds to the
    /// two's-complement of the operand (truncated to 16 bits) for
    /// constants.
    Neg,
    /// Logical not. Emits the 4-instruction `neg / sbb / inc` idiom at
    /// runtime; folds `0` → `1`, anything else → `0`.
    Not,
    /// Bitwise complement. Emits `not ax`; folds to `~x` (truncated).
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    /// C `>>`. For signed `int` we emit `sar`; for unsigned types
    /// (when we add them) we'll emit `shr`.
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    /// True for the six comparison operators (`==`, `!=`, `<`, `<=`,
    /// `>`, `>=`). They are produced as a 0/1 value when used in an
    /// expression context, and as a conditional jump when used as a
    /// condition.
    #[must_use]
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge
        )
    }

    /// The conditional-jump mnemonic to use when this comparison
    /// operator's result is **false**. For example, `<` is "less-than"
    /// — its inverse-on-false is "jump if not less", `jge`.
    /// Returns `None` for non-comparison operators.
    #[must_use]
    pub fn jump_if_false(self) -> Option<&'static str> {
        Some(match self {
            Self::Eq => "jne",
            Self::Ne => "je",
            Self::Lt => "jge",
            Self::Le => "jg",
            Self::Gt => "jle",
            Self::Ge => "jl",
            _ => return None,
        })
    }

    /// The conditional-jump mnemonic to use when this comparison
    /// operator's result is **true**. Used by `while` (the bottom-of-loop
    /// jump goes back to the body when the condition holds).
    /// Returns `None` for non-comparison operators.
    #[must_use]
    pub fn jump_if_true(self) -> Option<&'static str> {
        Some(match self {
            Self::Eq => "je",
            Self::Ne => "jne",
            Self::Lt => "jl",
            Self::Le => "jle",
            Self::Gt => "jg",
            Self::Ge => "jge",
            _ => return None,
        })
    }
}
