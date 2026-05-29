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
    /// Declared return type. Defaults to `Int` for the historical
    /// fixtures that all return int; first non-int return type
    /// (fixture 212, `long get()`) introduces variation here. Drives
    /// the codegen at `return <expr>;` so the value is materialized
    /// in the right register(s) for the ABI.
    pub ret_ty: Type,
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
    /// `static` prefix on the definition. Static functions are
    /// emitted in `_TEXT` like any other function but don't get a
    /// `public` declaration (fixture 499).
    pub is_static: bool,
    /// `pascal` calling convention: caller pushes args left-to-right
    /// (vs C's right-to-left), callee cleans the stack with `ret N`,
    /// and the symbol name is emitted UPPERCASE without the leading
    /// underscore. Fixture 1653.
    pub is_pascal: bool,
    /// `far` calling convention: caller pushes CS before the IP
    /// (simulates a far call), callee returns with `retf` (`cb`)
    /// instead of `ret` (`c3`), and the first param sits at
    /// [bp+6] (vs [bp+4] for near) because of the extra CS slot.
    /// Fixture 1654.
    pub is_far: bool,
    /// `near` calling convention explicitly requested. Only
    /// load-bearing under far-code memory models (medium / large /
    /// huge), where it overrides the default-far behavior so the
    /// function returns with `ret` and the caller stays at
    /// `[bp+4]`. Fixture 2061.
    pub is_near: bool,
    /// `interrupt` function: BCC emits a hardware-IRQ-safe wrapper
    /// — save all GP regs (AX/BX/CX/DX), segment regs (ES/DS),
    /// SI/DI, then BP; load DS = DGROUP so subsequent data accesses
    /// resolve correctly; epilogue restores in reverse and exits
    /// with `iret` (CF) instead of `ret`. Fixture 1655.
    pub is_interrupt: bool,
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
    /// `;` — an empty statement. Produces no asm. Used as a
    /// placeholder body in `for(init; cond; step) ;` (fixture 522).
    Empty,
    /// `<type> <name> [= <init>];`. For now only `int` and a single
    /// declarator with an optional initializer. `is_static = true` means
    /// the parser hoisted a synthetic `Global` for this name; codegen
    /// must skip slot allocation, register use-counting, and initialization
    /// code — references resolve through `GlobalTable` instead.
    /// `is_register = true` mirrors the C `register` storage class — the
    /// locals allocator lowers its enregister threshold so even a 1-use
    /// local gets a register slot.
    Declare {
        ty: Type,
        name: String,
        init: Option<Expr>,
        is_static: bool,
        is_register: bool,
        is_volatile: bool,
    },
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
    /// `<base>.<field>[<i>] = <value>;` — write to an array element
    /// inside a struct field. Codegen folds `<base>` to a base symbol
    /// or stack offset, then adds the field offset and constant index
    /// to land at a single memory destination. Fixture 497.
    MemberArrayAssign {
        base: String,
        field: String,
        indices: Vec<Expr>,
        value: Expr,
    },
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
        /// True when this node came from a postfix `lv++` / `lv--`
        /// (statement-position, result discarded). False for the
        /// prefix `++lv` form and for explicit `lv += K`. BCC's
        /// byte-target codegen picks memory-direct `inc/dec byte
        /// ptr <dest>` only for this case; everything else takes
        /// the AL load-modify-store detour. Fixture 702 (`g++` on
        /// char global was the first probe; member/array/deref
        /// siblings show the same asymmetry).
        from_postfix: bool,
    },
    /// `*<target> <op>= <value>;` — compound assignment through a
    /// dereferenced pointer. Sibling of `DerefAssign`; same `add word
    /// ptr [reg], imm8` shape `MemberCompoundAssign` produces, just
    /// with no field offset (fixture 183).
    DerefCompoundAssign {
        target: Expr,
        op: BinOp,
        value: Expr,
        /// See `MemberCompoundAssign::from_postfix`.
        from_postfix: bool,
    },
    /// `a[<i1>][<i2>]... <op>= <value>;` — compound assignment on an
    /// array element. Indices follow the same outermost-to-innermost
    /// convention as `ArrayAssign`. With all-constant indices BCC
    /// emits a single `<op> <width> ptr [bp-N], imm` (fixture 184).
    ArrayCompoundAssign {
        array: String,
        indices: Vec<Expr>,
        op: BinOp,
        value: Expr,
        /// See `MemberCompoundAssign::from_postfix`.
        from_postfix: bool,
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
    /// `goto <label>;` — unconditional jump to a labeled statement
    /// elsewhere in the function. Lowered to a single `jmp short
    /// <label>` (fixture 434). The target must be a `Label` statement
    /// in the same function body.
    Goto { label: String },
    /// `asm <body>` / `asm { <body> }` — inline assembly. `body`
    /// is the raw source text the lexer captured; the codegen
    /// splits it into individual asm lines (by `;` and `\n`) at
    /// emit time and substitutes C identifier references against
    /// the function's locals / params / globals. Fixtures 2303,
    /// 2304, 2120, 2119, 2122.
    Asm { body: String },
    /// `<name>:` — label that names a position in the function so
    /// `goto <name>;` can jump to it. The label itself emits no
    /// bytes; codegen attaches the asm-label to the *next* emitted
    /// instruction so the `jmp short` resolves to that position
    /// (fixture 434).
    Label { name: String },
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
    /// Bare `{ ... }` block at statement position. Introduces a
    /// new scope: locals declared inside don't leak out, and their
    /// stack slots can be reused by following sibling blocks
    /// (fixture 1966's two `int` blocks share `[bp-2]`).
    Block(Vec<Stmt>),
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
    /// `unsigned char` — 1-byte unsigned. Same byte layout as `Char`
    /// for storage and load; differs at int-promotion (`mov ah, 0`
    /// zero-extend vs. `cbw` sign-extend) and at compare (unsigned
    /// jump mnemonic family).
    UChar,
    /// `long` / `long int` — 32-bit signed. Stored as a high/low word
    /// pair (DX:AX in registers; two adjacent words in memory, low
    /// word first / little-endian).
    Long,
    /// `unsigned long` — 32-bit unsigned. Same byte layout as `Long`;
    /// only comparisons, shifts, and division care about the
    /// signedness.
    ULong,
    /// `float` — 32-bit IEEE 754 single-precision. Locals are 4 bytes,
    /// 2-byte aligned. Codegen routes arithmetic through 8087 FPU
    /// instructions (`fld`/`fstp`/`fadd`/etc.) and float→int conversions
    /// through the `N_FTOL@` runtime helper.
    Float,
    /// `double` — 64-bit IEEE 754 double-precision. 8 bytes, 2-byte
    /// aligned. Shares the same FPU codegen path as `Float`; the
    /// operand size on `fld`/`fstp` is `qword` instead of `dword`.
    Double,
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
    /// `T far *p` / `T huge *p` — a 32-bit far pointer (segment:offset)
    /// to a `T`. 4 bytes, 2-byte aligned, never register-resident.
    /// `is_huge` distinguishes `huge` (arithmetic normalizes the
    /// seg:off pair via runtime helpers) from plain `far` (arithmetic
    /// only modifies the offset half and may overflow at segment
    /// boundaries). Also produced implicitly for `int *` under
    /// compact / large / huge memory models — data pointers under
    /// those models default to far. Fixtures 1649, 1652, 2058, 2250
    /// (deref / write / postinc, far + huge equivalent shape),
    /// 1768 / 1667 (compact / large implicit far).
    FarPointer {
        pointee: Box<Type>,
        is_huge: bool,
    },
    /// `T near *p` — parser-side marker for a pointer the source
    /// explicitly tagged `near`. Under far-data memory models
    /// (compact, large, huge) the post-parse promotion pass needs
    /// to *skip* these locals so the user's override is honored
    /// (fixture 1748: `int near *p` in large model stays a 2-byte
    /// near pointer). The promotion pass collapses every
    /// `NearPointer` back to `Pointer` after deciding which
    /// unqualified pointers to promote, so codegen never sees this
    /// variant.
    NearPointer(Box<Type>),
    /// Parser-side marker for a function-pointer declarator
    /// (`int (*fp)(int)`). The post-parse promotion uses this to
    /// decide whether the slot is a 4-byte far pointer
    /// (medium / large / huge — code is far) or a 2-byte near
    /// pointer (tiny / small / compact — code is near). Codegen
    /// never sees this variant; the promotion collapses it to
    /// `FarPointer` or `Pointer`. Fixture 2211 (`int (*fp)(int) =
    /// dbl;` under -mm).
    FnPointer,
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
    /// Bitfield metadata, present only for `<type> <name> : <width>;`
    /// declarations. `None` for normal fields. The `offset` (above)
    /// is the byte offset of the storage container; `bit_offset`
    /// counts bits from the LSB of that byte. BCC packs first-
    /// declared bitfields into the LOWEST bits and grows upward
    /// (LSB-first / little-endian bit order).
    pub bitfield: Option<BitfieldInfo>,
    pub ty: Type,
    /// Byte offset of this field within the containing struct.
    pub offset: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitfieldInfo {
    /// Bit offset within the storage byte at `offset` (0..=7).
    /// A bitfield that crosses a byte boundary still records the
    /// start position here; codegen detects the spill at access
    /// time by comparing `bit_offset + bit_width` against 8.
    pub bit_offset: u8,
    /// Declared width in bits (1..=16 for unsigned int bitfields
    /// under BCC's small model).
    pub bit_width: u8,
}

impl Type {
    /// Size in bytes for stack-frame allocation. Memory-model-specific
    /// details (far pointers, etc.) will come later.
    #[must_use]
    pub fn size_bytes(&self) -> u16 {
        match self {
            Self::Int | Self::UInt => 2,
            Self::Char | Self::UChar => 1,
            Self::Long | Self::ULong => 4,
            Self::Float => 4,
            Self::Double => 8,
            Self::Array { elem, len } => {
                let elem = elem.size_bytes();
                elem * u16::try_from(*len).expect("array byte size fits in u16")
            }
            Self::Pointer(_) => 2,
            Self::NearPointer(_) => 2,
            Self::FarPointer { .. } => 4,
            // FnPointer is parser-side only; it gets rewritten to
            // Pointer or FarPointer before the locals layout pass
            // ever calls `size_bytes`. 2 is the safe default for
            // the small-model path that doesn't trigger promotion.
            Self::FnPointer => 2,
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
            Self::Char | Self::UChar => 1,
            Self::Long | Self::ULong => 2,
            Self::Float | Self::Double => 2,
            Self::Array { elem, .. } => elem.alignment(),
            Self::Pointer(_) => 2,
            Self::NearPointer(_) => 2,
            Self::FarPointer { .. } => 2,
            Self::FnPointer => 2,
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
    /// (`jl/jge/jle/jg`).
    #[must_use]
    pub fn is_unsigned(&self) -> bool {
        matches!(self, Self::UInt | Self::ULong | Self::UChar)
    }

    /// True for the 1-byte char family (signed `char` and
    /// `unsigned char`). Storage, load, and assign operations are
    /// identical between the two; only int-promotion (sign-extend
    /// vs. zero-extend) and comparison (signed vs. unsigned mnemonic
    /// family) differ.
    #[must_use]
    pub fn is_char_like(&self) -> bool {
        matches!(self, Self::Char | Self::UChar)
    }

    /// Whether this type is a 32-bit long-family type (signed or
    /// unsigned). Useful where the byte-level emission is identical
    /// between `long` and `unsigned long` (arithmetic, in-memory
    /// layout) and only the comparison/shift mnemonic family cares
    /// about signedness.
    #[must_use]
    pub fn is_long_like(&self) -> bool {
        matches!(self, Self::Long | Self::ULong)
    }

    /// Recursively check whether this type contains a `long`-like
    /// type anywhere (the type itself, or as an array element, or
    /// inside a struct field). Used by the publics-ordering rule
    /// in `emit_s.rs` to detect "any long global present"
    /// (fixture 829).
    #[must_use]
    pub fn contains_long(&self) -> bool {
        match self {
            Self::Long | Self::ULong => true,
            Self::Array { elem, .. } => elem.contains_long(),
            Self::Struct { fields, .. } => fields.iter().any(|f| f.ty.contains_long()),
            _ => false,
        }
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
    /// True for `int`/`unsigned int` and any pointer. UInt has the
    /// same 2-byte width and register-pool eligibility as Int — the
    /// signedness difference only shows up in arithmetic mnemonics
    /// (jl vs. jb), which the using codegen sites pick separately.
    /// Arrays never enregister.
    #[must_use]
    pub fn is_int_like(&self) -> bool {
        matches!(self, Self::Int | Self::UInt | Self::Pointer(_))
    }

    /// True for floating-point types (`float`, `double`). They never
    /// enregister; arithmetic routes through the 8087 FPU.
    #[must_use]
    pub fn is_float_like(&self) -> bool {
        matches!(self, Self::Float | Self::Double)
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
        match self {
            Self::Pointer(inner) | Self::NearPointer(inner) => Some(inner),
            Self::FarPointer { pointee, .. } => Some(pointee),
            // FnPointer is opaque — codegen never asks for its
            // pointee (the function signature isn't modeled), and
            // by the time anyone could it's already been rewritten.
            Self::FnPointer => None,
            _ => None,
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
    /// 32-bit float literal as raw IEEE 754 bits. Codegen pools the
    /// bytes in the `s@` constant area and emits a `fld dword ptr
    /// DGROUP:s@+<offset>` to load the value onto the FPU stack.
    FloatLit(u32),
    /// 64-bit double literal as raw IEEE 754 bits. Pooled the same way
    /// as `FloatLit`; loaded via `fld qword ptr` in expression context,
    /// though a constant assigned to a float local is loaded as `dword
    /// ptr` (BCC promotes via the FPU's 80-bit internal width before
    /// the `fstp` truncates back to 64).
    DoubleLit(u64),
    Ident(String),
    /// Pseudo-register reference: `_AX`, `_BX`, ..., `_DH`, `_SI`,
    /// ..., `_DS`. Parsed as a bare identifier and then rewritten
    /// to this variant by `rewrite_pseudo_registers` so the existing
    /// `Ident` peephole recognizers — which would query `Locals` and
    /// panic — don't fire. Codegen for this variant lives in one
    /// place per emit path. See `is_asm_pseudo_register` for the set.
    PseudoReg(String),
    BinOp { op: BinOp, left: Box<Expr>, right: Box<Expr> },
    /// Prefix unary operator: `-e`, `!e`, `~e`.
    Unary { op: UnaryOp, operand: Box<Expr> },
    /// Short-circuiting logical operator: `a && b`, `a || b`. Held
    /// separately from `BinOp` because the right operand may not be
    /// evaluated, and the codegen patterns are completely different
    /// from the regular binary ops.
    Logical { op: LogicalOp, left: Box<Expr>, right: Box<Expr> },
    /// `++name` / `--name` / `name++` / `name--`. The target must
    /// be a bare identifier referring to a local or parameter. This
    /// is the common shape; for ++/-- on a more general lvalue
    /// (`(*pp)++`, `(arr[i])++`), the parser uses `UpdateLvalue`.
    Update { target: String, op: UpdateOp, position: UpdatePosition },
    /// Postfix or prefix ++/-- on an arbitrary lvalue expression.
    /// Today this is parser-emitted only for paren-wrapped targets
    /// (`(*pp)++`, `(p->f)++`); bare-identifier targets continue to
    /// use `Update` so the existing codegen sites that pattern-match
    /// on `target: String` stay untouched. Fixture 3662
    /// (`*(*pp)++` for an `int **pp`).
    UpdateLvalue { target: Box<Expr>, op: UpdateOp, position: UpdatePosition },
    /// `name = value` as an expression. In C, assignment is an
    /// expression that yields the assigned value; we have it as a
    /// statement (`StmtKind::Assign`) for the common case, and this
    /// expression form covers `for (i = 0; ...; ...)` init/step
    /// clauses where the assignment appears in expression position.
    AssignExpr { target: String, value: Box<Expr> },
    /// Lvalue-assign expression: `<lvalue> = <value>` where the
    /// lvalue is something other than a bare identifier (e.g.
    /// `*p`, `a[i]`, `p->x`). Distinct from `AssignExpr` because
    /// the codegen path is "evaluate value into AX, then store to
    /// the lvalue's address" — the same store path as
    /// `StmtKind::DerefAssign`/`ArrayAssign` but the expression
    /// continues to use the value in AX. Fixtures 3333
    /// (`(*p = 5) + 1`), 1986 (`v = (a[i] = 42)`), 1808
    /// (`while (*d++ = *s++) ...`).
    AssignLvalueExpr { target: Box<Expr>, value: Box<Expr> },
    /// `name <op>= value` as an expression — the for-clause /
    /// argument-position compound assignment. Distinct from
    /// AssignExpr because BCC emits different bytes for
    /// `i = i + 2` (AX-route assign) vs `i += 2` (direct
    /// register inc). Statement-level compound assigns go through
    /// `StmtKind::CompoundAssign` instead. Fixtures 1328, 3150-3161.
    CompoundAssignExpr { target: String, op: BinOp, value: Box<Expr> },
    /// Direct function call by name.
    Call { name: String, args: Vec<Expr> },
    /// Indirect function call through a function pointer obtained
    /// from an arbitrary expression (currently `<arr>[<idx>](args)`
    /// for function-pointer arrays). The callee address comes from
    /// evaluating `addr` and treating it as a function pointer.
    /// Fixtures 2308, 2435, 2944, 3481, 3696.
    CallVia { addr: Box<Expr>, args: Vec<Expr> },
    /// `&<name>` — address-of a named local or parameter. Restricted
    /// to a bare ident today; the more general `&<lvalue>` (e.g.
    /// `&a[i]`) doesn't appear in fixtures yet.
    AddressOf(String),
    /// `&<array>[<const>]` — address of a specific array element with
    /// a compile-time constant index. The parser pre-computes
    /// `byte_offset = index * sizeof(elem)` so codegen just emits the
    /// label-plus-offset form (e.g. `DGROUP:_arr+2`). Currently only
    /// fixture 198's file-scope `int *p = &arr[1];` exercises this.
    AddressOfArrayElem { array: String, byte_offset: i32 },
    /// `&<array>[<var>]` — address of an array element with a
    /// non-constant index. Carries the index expression and the
    /// element stride so codegen can scale at runtime. Distinct
    /// from the constant-index variant because we can't fold the
    /// offset into the symbol reference. Fixtures 3249, 3645.
    AddressOfArrayElemVar { array: String, index: Box<Expr>, elem_size: u16 },
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
    /// `{ <expr>, <expr>, ... }` — a brace-enclosed initializer list,
    /// only valid as the RHS of a Declare/Global with an aggregate
    /// type. Items are emitted in source order against the target's
    /// element type. No runtime semantics — codegen consumes this
    /// only at file-scope-init time today (fixture 189).
    InitList { items: Vec<Expr> },
    /// `<left>, <right>` — comma operator at expression level. Left
    /// evaluates for side effects; right's value is the expression's
    /// value. Only appears in contexts where the C grammar permits a
    /// *comma-expression* (parenthesized, or at the top of an
    /// expression statement). Inside argument lists or initializer
    /// braces the comma is a separator, not the operator. Fixture
    /// 469.
    Comma { left: Box<Expr>, right: Box<Expr> },
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

    /// True for binary operators where `a OP b == b OP a` (`+`, `*`,
    /// `&`, `|`, `^`, `==`, `!=`). BCC swaps operands when the left
    /// is constant-foldable and the right isn't — `3 + *p` emits as
    /// `*p + 3` so the harder-to-materialize operand lands in AX.
    #[must_use]
    pub fn is_commutative(self) -> bool {
        matches!(
            self,
            Self::Add | Self::Mul | Self::BitAnd | Self::BitOr | Self::BitXor | Self::Eq | Self::Ne
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
