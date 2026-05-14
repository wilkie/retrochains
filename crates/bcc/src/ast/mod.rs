//! AST types. Faithful to source order; every node carries the byte span
//! it covers so codegen can find the original source line(s) that backed
//! it (BCC interleaves source lines as `;` comments — we need the
//! original bytes to reproduce that).

use crate::lex::Span;

/// A whole translation unit (one `.C` file). Holds the top-level
/// declarations in source order so codegen can preserve that order
/// when emitting public symbols and segment data.
#[derive(Debug)]
pub struct Unit {
    pub functions: Vec<Function>,
    /// File-scope variable declarations. Source order preserved;
    /// codegen partitions them into `_DATA` (initialized) and `_BSS`
    /// (uninitialized) at emit time.
    pub globals: Vec<Global>,
    /// Source order of top-level decls, so the LIFO `public` list at
    /// the end of the file can be reconstructed (LIFO of declaration
    /// order). Each entry references either a function or a global by
    /// index into the corresponding vec.
    pub decl_order: Vec<TopLevelRef>,
}

/// One file-scope variable. `init = None` means the global lives in
/// `_BSS`; `Some(...)` means `_DATA`. `is_static` suppresses the
/// trailing `public _<name>` directive — the symbol stays private to
/// this translation unit but is still allocated in DGROUP.
/// `is_extern` means the storage is defined in some other TU; we
/// emit only an `extrn _<name>:<width>` declaration and no slot.
#[derive(Debug)]
pub struct Global {
    pub name: String,
    pub ty: Type,
    pub init: Option<Expr>,
    pub is_static: bool,
    pub is_extern: bool,
    pub span: Span,
}

/// Source-order pointer back into a `Unit`'s `functions` or `globals`
/// vector. Used to walk top-level items in declaration order.
#[derive(Debug, Clone, Copy)]
pub enum TopLevelRef {
    Function(usize),
    Global(usize),
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    /// Parameters in source order. The empty list represents `(void)`
    /// — C's "no parameters" spelling — not a variadic prototype.
    pub params: Vec<Param>,
    /// The full byte range of the function definition, from the
    /// `int main…` opening through the closing `}` (or `;` for a
    /// prototype). Used to compute which source lines this function
    /// spans, which in turn drives the `;` source-comment emission
    /// in the asm.
    pub span: Span,
    /// `Some(body)` for a definition; `None` for a prototype-only
    /// declaration (`int puts(char *s);` — fixture 097). Prototypes
    /// don't produce asm output of their own, but they do feed the
    /// signature table so call sites know each parameter's type.
    pub body: Option<Vec<Stmt>>,
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
    /// declarator with an optional initializer. `is_static = true` means
    /// the parser hoisted a synthetic `Global` for this name; codegen
    /// must skip slot allocation, register use-counting, and initialization
    /// code — references resolve through `GlobalTable` instead.
    Declare { ty: Type, name: String, init: Option<Expr>, is_static: bool },
    /// `if (cond) then-body [else else-body]`.
    If { cond: Expr, then_branch: Vec<Stmt>, else_branch: Option<Vec<Stmt>> },
    /// `<name> = <value>;`. The bare-ident assignment form. Pointer-
    /// indirect (`*p = v;`) and array-index (`a[i] = v;`) targets get
    /// their own statement kinds because the asm shape is distinct
    /// enough that lumping them into one node would just split into a
    /// match at codegen anyway.
    Assign { name: String, value: Expr },
    /// `a[<i1>][<i2>]... = <value>;`. `array` is the base name of a
    /// local of array type; `indices` holds the subscripts from
    /// outermost to innermost (so `a[1][2]` has `indices = [1, 2]`).
    /// Single-dim access still parses as a one-element `indices` vec.
    /// Codegen specializes the all-constant-indices case to a direct
    /// stack-offset store.
    ArrayAssign { array: String, indices: Vec<Expr>, value: Expr },
    /// `*<target> = <value>;`. `target` is the pointer expression
    /// — usually an `Ident` for a pointer local, but in principle
    /// anything that evaluates to a pointer.
    DerefAssign { target: Expr, value: Expr },
    /// `<base>.<field> = <value>;` or `<base>-><field> = <value>;`.
    /// The `kind` distinguishes the two source forms — codegen uses
    /// it to decide whether the base is a struct directly (for `.`)
    /// or a pointer to a struct (for `->`).
    MemberAssign {
        base: Expr,
        field: String,
        kind: MemberKind,
        value: Expr,
    },
    /// `<base>.<field> <op>= <value>;` or `<base>-><field> <op>= ...`.
    /// Same shape as `MemberAssign` but the field gets a read-modify-
    /// write at its effective address (fixture 182's `p->x += 5` →
    /// `add word ptr [si], 5`).
    MemberCompoundAssign {
        base: Expr,
        field: String,
        kind: MemberKind,
        op: BinOp,
        value: Expr,
    },
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
    /// `for (init; cond; step) body`. Any clause may be omitted (C
    /// lets you leave it blank). `init` and `step` are vectors so the
    /// comma operator at the top of those clauses parses as a sequence
    /// of expressions evaluated for their side effects, matching the
    /// `for (i=0, j=10; ...; i++, j--)` idiom (fixture 172). Empty
    /// clauses become `None`.
    For {
        init: Option<Vec<Expr>>,
        cond: Option<Expr>,
        step: Option<Vec<Expr>>,
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
    /// `unsigned` / `unsigned int` — 16-bit unsigned. Same byte layout
    /// as `Int`; only comparisons and (eventually) shifts care about
    /// the signedness.
    UInt,
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
    /// `struct <tag>? { <fields> }` — fields packed tightly with no
    /// inter-field padding (so a `char` at offset 0 followed by an
    /// `int` lands the int at offset 1, fixture 102). The total
    /// size rounds up to a 2-byte multiple, which the parser bakes
    /// into the recorded size at construction time. Anonymous
    /// structs have `name: None`. Fixture 104's typedef of an
    /// anonymous struct round-trips through this representation.
    Struct {
        name: Option<String>,
        fields: Vec<StructField>,
        /// Pre-computed total byte size including end-padding. Cached
        /// here so `size_bytes()` doesn't have to re-walk the fields.
        size: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructField {
    pub name: String,
    pub ty: Type,
    /// Byte offset of this field within the containing struct.
    pub offset: u16,
}

impl Type {
    /// Size in bytes for stack-frame allocation. Memory-model-specific
    /// details (far pointers, etc.) will come later.
    #[must_use]
    pub fn size_bytes(&self) -> u16 {
        match self {
            Self::Int | Self::UInt => 2,
            Self::Char => 1,
            Self::Array { elem, len } => {
                let elem = elem.size_bytes();
                elem * u16::try_from(*len).expect("array byte size fits in u16")
            }
            Self::Pointer(_) => 2,
            Self::Struct { size, .. } => *size,
        }
    }

    /// Alignment (in bytes) required for this type's stack slot. `int`
    /// must land on an even bp-offset (so BCC pads when a preceding
    /// `char` left the cursor at an odd offset — see fixture 011).
    /// Arrays align like their element type; pointers like ints.
    #[must_use]
    pub fn alignment(&self) -> u16 {
        match self {
            Self::Int | Self::UInt => 2,
            Self::Char => 1,
            Self::Array { elem, .. } => elem.alignment(),
            Self::Pointer(_) => 2,
            // Struct alignment: 2 (word). The size rounding to even
            // is part of the per-struct size computation, so this is
            // mostly a placement-alignment hint when a struct is
            // followed by another local.
            Self::Struct { .. } => 2,
        }
    }

    /// True for `unsigned` integer types. Comparisons between operands
    /// where at least one side is unsigned use the unsigned-jump
    /// mnemonic family (`jb/jae/jbe/ja`) instead of the signed
    /// (`jl/jge/jle/jg`). `char` is signed in BCC; `unsigned char`
    /// would be a separate variant when a fixture demands it.
    #[must_use]
    pub fn is_unsigned(&self) -> bool {
        matches!(self, Self::UInt)
    }

    /// Look up a field by name. Returns the field's offset and type
    /// (cloned), or `None` if this isn't a struct or the field name
    /// isn't present.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<(u16, Type)> {
        let Self::Struct { fields, .. } = self else { return None };
        fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| (f.offset, f.ty.clone()))
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
    /// `<array>[<index>]` in an rvalue context. `array` can be an
    /// `Ident` (a local of array type) or a `StringLit` (a string
    /// literal indexed in place, e.g. `"hi"[0]`). Codegen specializes
    /// constant indices and folds `StringLit`-base accesses to a
    /// direct `DGROUP:s@<offset>` memory reference.
    ArrayIndex { array: Box<Expr>, index: Box<Expr> },
    /// `"...."` — a string literal. The bytes are the decoded
    /// contents (escapes resolved); codegen appends a trailing NUL
    /// when materializing the literal into the `s@` block.
    StringLit(Vec<u8>),
    /// `<base>.<field>` or `<base>-><field>` in rvalue position.
    /// `kind` distinguishes the syntactic form: `Dot` means base is
    /// a struct directly (compute &struct + offset), `Arrow` means
    /// base is a pointer-to-struct (load through pointer + offset).
    Member {
        base: Box<Expr>,
        field: String,
        kind: MemberKind,
    },
    /// `<cond> ? <then-value> : <else-value>` — the conditional
    /// (ternary) expression. The same skeleton an `if-else` produces,
    /// but with arms loading values into AX instead of running
    /// statements. Each ternary node reserves 3 label slots; the
    /// false-arm and merge labels land at base+1 and base+2.
    Ternary {
        cond: Box<Expr>,
        then_value: Box<Expr>,
        else_value: Box<Expr>,
    },
    /// `(<type>) <operand>` — an explicit type cast. Today we only
    /// pin the narrowing case (int → char, fixture 170); widening
    /// (char → int) requires the same `cbw` we emit when reading a
    /// char local, so it lowers identically.
    Cast { ty: Type, operand: Box<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    /// `a.x` — base is a struct value (typically a stack local).
    Dot,
    /// `p->x` — base is a pointer to a struct.
    Arrow,
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
    /// Returns `None` for non-comparison operators. `unsigned` selects
    /// the `jb/jae/jbe/ja` family BCC uses when at least one operand
    /// of an ordered comparison has unsigned type.
    #[must_use]
    pub fn jump_if_false(self, unsigned: bool) -> Option<&'static str> {
        Some(match (self, unsigned) {
            (Self::Eq, _) => "jne",
            (Self::Ne, _) => "je",
            (Self::Lt, false) => "jge",
            (Self::Le, false) => "jg",
            (Self::Gt, false) => "jle",
            (Self::Ge, false) => "jl",
            (Self::Lt, true) => "jae",
            (Self::Le, true) => "ja",
            (Self::Gt, true) => "jbe",
            (Self::Ge, true) => "jb",
            _ => return None,
        })
    }

    /// The conditional-jump mnemonic to use when this comparison
    /// operator's result is **true**. Used by `while` (the bottom-of-loop
    /// jump goes back to the body when the condition holds).
    /// Returns `None` for non-comparison operators.
    #[must_use]
    pub fn jump_if_true(self, unsigned: bool) -> Option<&'static str> {
        Some(match (self, unsigned) {
            (Self::Eq, _) => "je",
            (Self::Ne, _) => "jne",
            (Self::Lt, false) => "jl",
            (Self::Le, false) => "jle",
            (Self::Gt, false) => "jg",
            (Self::Ge, false) => "jge",
            (Self::Lt, true) => "jb",
            (Self::Le, true) => "jbe",
            (Self::Gt, true) => "ja",
            (Self::Ge, true) => "jae",
            _ => return None,
        })
    }
}
