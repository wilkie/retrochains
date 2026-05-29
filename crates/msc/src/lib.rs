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
    /// Named struct definitions (`struct S { ... };`) collected at
    /// parse time. Used to resolve `s.field` and `p->field` to byte
    /// offsets and to size struct locals/globals.
    pub structs: Vec<StructDef>,
    pub functions: Vec<Function>,
    /// Top-level declarations in source order. Used by PUBDEF
    /// emission, which groups consecutive same-segment symbols into
    /// one record and starts a new record on bucket changes
    /// (fixture 4125's `_get`/`_g`/`_main` interleave).
    pub decl_order: Vec<TopDecl>,
    /// Each string is the bytes between the source double-quotes
    /// PLUS a terminating NUL byte appended by the parser.
    pub strings: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
pub enum TopDecl {
    Global(usize),
    Function(usize),
}

/// A file-scope global variable. Phase 1 covers scalar `int g [= K];`
/// and array `int a[N];` (uninit only so far — fixture 4107).
#[derive(Debug, Clone)]
pub struct Global {
    pub name: String,
    /// `Some(vec)` for an explicit initializer. Each slot may be a
    /// byte (`char` array element) or a 2-byte word (everything
    /// else). `None` is the tentative form (`int g;` / `int a[N];`)
    /// which lowers to a COMDEF (fixtures 4105, 4107).
    pub init: Option<Vec<GlobalInit>>,
    /// Array element count. `1` for scalar `int g;`. Multiplied by
    /// `element_size` to yield the COMDEF or _DATA byte length.
    pub array_len: usize,
    /// Bytes per element (1 for char arrays, 2 for everything else
    /// in Phase 1).
    pub element_size: usize,
    /// `true` for declared pointer types (`int *`, `char *`). The
    /// storage is still 2 bytes (near pointer), but indexing a
    /// pointer requires a load+offset, not a direct addressing mode.
    pub is_pointer: bool,
    /// `Some(idx)` when the global is `struct S` (not pointer). The
    /// field metadata for member access lives in Unit::structs.
    pub struct_idx: Option<usize>,
    /// `true` for `long` globals. They occupy 4 bytes of storage and
    /// init values split into low + high 16-bit halves. Plain int
    /// access reads the low half; high half is reserved for runtime
    /// long arithmetic that isn't implemented yet.
    pub is_long: bool,
}

impl Global {
    /// Bytes occupied in `_DATA` (init) or `_BSS`/COMDEF (tentative).
    /// Pointers are always 2 bytes per slot regardless of pointee
    /// size; arrays scale element_size by `array_len`.
    fn storage_bytes(&self) -> usize {
        let slot = if self.is_pointer { 2 } else { self.element_size };
        slot * self.array_len
    }
}

#[derive(Debug, Clone)]
pub enum GlobalInit {
    /// Plain int literal — stored as 16-bit LE in `_DATA`.
    Int(i32),
    /// One byte — used by `char a[N] = "...";` (fixture 4117).
    Byte(u8),
    /// CONST-segment string address — stored as a 2-byte placeholder
    /// with a FIXUP that the linker resolves to DGROUP:CONST+offset.
    /// `usize` indexes into `Unit::strings`. Fixture 4110.
    StrAddr(usize),
    /// _DATA-segment global's address — `int *q = &g;`. Placeholder
    /// is the target global's `_DATA` offset; FIXUP shape is the
    /// same `c4 off 9d` as a PUBDEF-global access. Fixture 4115.
    GlobalAddr(usize),
}

impl GlobalInit {
    fn size_bytes(&self) -> usize {
        match self {
            GlobalInit::Byte(_) => 1,
            _ => 2,
        }
    }
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
    pub locals: Vec<LocalSpec>,
    pub body: Vec<Stmt>,
}

/// A function-local variable's storage descriptor. `size` is bytes
/// per element (1 for `char`, 2 for `int` / pointer). `array_len`
/// is 1 for scalars and N for `T name[N]`. `init` is the optional
/// scalar initializer used by const-prop and the prologue-store path
/// — array inits live in a separate prelude of synthesized stores.
#[derive(Debug, Clone)]
pub struct LocalSpec {
    pub size: usize,
    pub array_len: usize,
    pub init: Option<i32>,
    /// `Some(struct_idx)` when the local is `struct S name;`.
    /// The struct's fields define member-access offsets and the
    /// overall storage_bytes is the struct's total size (not
    /// size * array_len).
    pub struct_idx: Option<usize>,
    /// True for `long x;` — storage is two word slots (low at the
    /// shallower disp, high at disp+2). Init writes both halves.
    pub is_long: bool,
    /// True when the init came from a pure literal/constant expression
    /// with no Local references. For char locals only this flag
    /// distinguishes `char c = 'A' + 1;` (folds for read) from
    /// `char c = a + b;` (stored, but read from slot).
    pub init_is_literal: bool,
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<StructField>,
    pub total_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub name: String,
    pub byte_off: u16,
    pub size: u8,
}

impl LocalSpec {
    pub fn int(init: Option<i32>) -> Self {
        Self { size: 2, array_len: 1, init, struct_idx: None, is_long: false, init_is_literal: init.is_some() }
    }
    pub fn char_(init: Option<i32>) -> Self {
        Self { size: 1, array_len: 1, init, struct_idx: None, is_long: false, init_is_literal: init.is_some() }
    }
    pub fn long_(init: Option<i32>) -> Self {
        Self { size: 2, array_len: 2, init, struct_idx: None, is_long: true, init_is_literal: init.is_some() }
    }
    /// Bytes occupied in the frame, rounded up to an even count.
    /// MSC pads each local to a word boundary — scalar char gets 2
    /// bytes, char[3] gets 4 bytes, int[3] gets 6 bytes. Fixture 1134.
    /// Struct locals carry the struct's natural total_bytes
    /// (also even-padded).
    pub fn storage_bytes(&self) -> usize {
        let raw = self.size * self.array_len;
        (raw + 1) & !1
    }
}

/// View of the function's locals shared across emit_* helpers. Carries
/// both the const-prop init values and per-local frame-displacement +
/// element-size data. Compute once in emit_function and pass by ref
/// everywhere the old `&[Option<i32>]` lived.
pub struct Locals<'a> {
    /// Const-prop view: `Some(K)` for locals known to hold a literal.
    pub inits: &'a [Option<i32>],
    /// `disps[i]` is the BP-relative displacement of local `i`'s
    /// first byte (element 0 for arrays). Always negative; -2 for
    /// the first declared scalar.
    pub disps: &'a [i16],
    /// Per-element size in bytes (1 for char, 2 otherwise).
    pub sizes: &'a [usize],
    /// Parallel-indexed flags marking globals that hold a 4-byte
    /// long. Used by global-assign codegen to emit both halves.
    pub long_globals: &'a [bool],
    /// Parallel-indexed flags marking locals that are `long`. Direct
    /// loads (return, assign) bypass the fold view so the slot is
    /// read at runtime even when its constant value is known.
    pub long_locals: &'a [bool],
    /// Parallel-indexed: true iff the local's init came from a pure
    /// literal expression. Char locals fold for bare reads only when
    /// this is true (fixture 1023 vs 1046).
    pub init_literals: &'a [bool],
}

impl Locals<'_> {
    pub fn get_init(&self, idx: usize) -> Option<i32> {
        self.inits.get(idx).copied().flatten()
    }
    pub fn disp(&self, idx: usize) -> i16 {
        self.disps[idx]
    }
    pub fn size(&self, idx: usize) -> usize {
        self.sizes[idx]
    }
    pub fn is_long_global(&self, idx: usize) -> bool {
        self.long_globals.get(idx).copied().unwrap_or(false)
    }
    pub fn is_long_local(&self, idx: usize) -> bool {
        self.long_locals.get(idx).copied().unwrap_or(false)
    }
    pub fn init_is_literal(&self, idx: usize) -> bool {
        self.init_literals.get(idx).copied().unwrap_or(false)
    }
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
    /// `{ <stmt>* }` — sequence of statements with no scoping
    /// effects of its own. Used as the body of if / loops when the
    /// source uses braces.
    Block(Vec<Stmt>),
    /// `switch (<expr>) { case K: ...; default: ...; }` — currently
    /// only supported when the scrutinee folds to a known literal at
    /// compile time. ConstProp picks the matching case and inlines
    /// its body (up to the next break) before codegen sees the switch.
    Switch {
        scrutinee: Expr,
        cases: Vec<SwitchArm>,
    },
    /// `break;` — short-circuit the enclosing switch / loop. Used
    /// only as a flow-control marker inside the const-folded switch
    /// case-walker; loop break isn't yet implemented.
    Break,
    /// `continue;` — Phase 1 placeholder; not yet implemented for
    /// loops, but parses so source files compile.
    Continue,
}

/// One arm of a `switch` statement. `value` is `Some(K)` for `case K:`
/// and `None` for `default:`. `body` runs until the next `break`.
#[derive(Debug, Clone)]
pub struct SwitchArm {
    pub value: Option<i32>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, Copy)]
pub enum AssignTarget {
    Local(usize),
    Global(usize),
    /// `*<ptr-global> = <expr>;` — store the RHS through a pointer
    /// global. Fixture 4116.
    DerefGlobal(usize),
    /// `*<ptr-local> = <expr>;` — store through a pointer local.
    /// Codegen: `mov bx, [bp-disp]; mov [bx], imm/ax`.
    DerefLocal(usize),
    /// `<global>[K] = <expr>;` — write a 2-byte word at a constant
    /// index into an int-array global. `byte_off` is `K * 2`.
    /// Fixture 4119.
    IndexedGlobal { array: usize, byte_off: u16 },
    /// `<char-global>[K] = <byte>;` — write one byte at a constant
    /// index into a char-array global. `byte_off` is `K`. Fixture 4122.
    IndexedGlobalByte { array: usize, byte_off: u16 },
    /// `<char-ptr-global>[K] = <byte>;` — write one byte through a
    /// char-pointer global. `disp` is the constant index (fits in
    /// disp8 in Phase 1). Fixture 4124.
    PtrIndexByte { ptr: usize, disp: i8 },
    /// `<struct-local>.<field> = <expr>;` — store to a struct field.
    LocalField { local: usize, byte_off: u16, size: u8 },
    /// `<struct-ptr-local>-><field> = <expr>;` — store through a
    /// struct pointer local.
    DerefLocalField { ptr_local: usize, byte_off: u16, size: u8 },
    /// `<struct-global>.<field> = <expr>;` — store to a global
    /// struct's field.
    GlobalField { global: usize, byte_off: u16, size: u8 },
    /// `<struct-ptr-param>-><field> = <expr>;` — store via a struct
    /// pointer parameter. Codegen: `mov bx, [bp+pdisp];
    /// c7 47 off imm16` (word) / `c6 47 off imm8` (byte).
    DerefParamField { ptr_param: usize, byte_off: u16, size: u8 },
    /// `<local-int-array>[K] = <expr>;` — write a word at a constant
    /// index. `byte_off` is `K * 2`. Codegen uses BP-rel store at
    /// `locals.disp(local) + byte_off`.
    IndexedLocal { local: usize, byte_off: u16 },
    /// `<local-char-array>[K] = <byte>;` — write a byte at a constant
    /// index. `byte_off` is `K`. Codegen uses `c6 46 disp imm8`.
    IndexedLocalByte { local: usize, byte_off: u16 },
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
    /// `if (a && b)` — short-circuit conjunction. The skip target
    /// from `a` jumps over `b` AND the body.
    And(Box<Cond>, Box<Cond>),
    /// `if (a || b)` — short-circuit disjunction. The take-then
    /// target from `a` jumps into the body, skipping `b`.
    Or(Box<Cond>, Box<Cond>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
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
    /// Array element access — `a[<expr>]` for word-sized elements.
    /// Constant index folds to an `a1 imm16` load whose immediate
    /// is `2 * index` (linker adds the array base via the FIXUP).
    /// Variable index uses `mov bx, ...; shl bx, 1; mov ax, [bx+addr]`.
    /// Fixtures 4109, 4112.
    Index { array: usize, index: Box<Expr> },
    /// Byte-sized array element access — `s[<expr>]` for `char`
    /// arrays. Constant index folds to `a0 imm16` (mov al, moffs8)
    /// + `98` (cbw) to widen into AX. Fixture 4121.
    IndexByte { array: usize, index: Box<Expr> },
    /// `<local-int-array>[<expr>]` — read a word at a runtime or
    /// constant index. Constant K lowers to `mov ax, [bp-disp+2K]`
    /// (`8b 46 disp8`). Variable index isn't lowered yet.
    LocalIndex { local: usize, index: Box<Expr> },
    /// `<local-char-array>[<expr>]` — read a byte + cbw. Constant K
    /// lowers to `mov al, [bp-disp+K]; cbw` (`8a 46 disp8; 98`).
    LocalIndexByte { local: usize, index: Box<Expr> },
    /// `<param>[<expr>]` — `int *p` / `int p[]` parameter index.
    /// Constant K lowers to `mov bx, [bp+param_disp]; mov ax, [bx+2K]`.
    ParamIndex { param: usize, index: Box<Expr> },
    /// `<struct-local>.<field>` — read a field of a struct local.
    /// `byte_off` is the precomputed field offset within the
    /// struct. `size == 1` triggers `mov al, [bp+disp]; cbw`;
    /// `size == 2` uses `mov ax, [bp+disp]`.
    LocalField { local: usize, byte_off: u16, size: u8 },
    /// `<struct-ptr-local>-><field>` — deref through a struct
    /// pointer local. Lowers to `mov bx, [bp+local_disp];
    /// mov ax, [bx+byte_off]` for word fields.
    DerefLocalField { ptr_local: usize, byte_off: u16, size: u8 },
    /// `<struct-global>.<field>` — read a field of a struct global.
    /// Lowers to `a1 disp+off` (word) or `a0 disp+off; 98` (byte).
    GlobalField { global: usize, byte_off: u16, size: u8 },
    /// `<struct-ptr-param>-><field>` — deref through a struct
    /// pointer parameter. `mov bx, [bp+param_disp]; mov ax, [bx+off]`.
    DerefParamField { ptr_param: usize, byte_off: u16, size: u8 },
    /// Pointer-indexed byte read — `p[<expr>]` where `p` is a
    /// `char *` global. Constant index lowers to
    /// `mov bx, [p]; mov al, [bx+disp]; cbw`. Fixture 4123.
    PtrIndexByte { ptr: usize, index: Box<Expr> },
    /// `&<global>` — address-of a file-scope global, as an
    /// expression. Lowers to `b8 imm16` with a FIXUP on the imm16
    /// targeting the global. Fixture 4125 (passed as an argument).
    AddrOfGlobal(usize),
    /// `&<local>` — address-of a stack local. Lowers to
    /// `lea ax, [bp-disp]` (`8d 46 disp`).
    AddrOfLocal(usize),
    /// `<cond> ? <then> : <else>` — C ternary. Folds when cond is
    /// a known literal; otherwise codegen would need branching
    /// support (not yet implemented).
    Ternary {
        cond: Box<Expr>,
        then_arm: Box<Expr>,
        else_arm: Box<Expr>,
    },
    /// `*<ptr>` — byte-sized pointer dereference (`char *`). Lowers
    /// to `mov bx, <ptr>; mov al, [bx]; cbw`. Fixture 4111.
    DerefByte { ptr: Box<Expr> },
    /// `*<ptr>` — word-sized pointer dereference (`int *`). Lowers
    /// to `mov bx, <ptr>; mov ax, [bx]`. Fixture 4125.
    DerefWord { ptr: Box<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
    LogAnd,
    LogOr,
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
                    BinOp::Div if r != 0 => l.wrapping_div(r),
                    BinOp::Mod if r != 0 => l.wrapping_rem(r),
                    BinOp::Div | BinOp::Mod => return None,
                    BinOp::Eq => i32::from(l == r),
                    BinOp::Ne => i32::from(l != r),
                    BinOp::Lt => i32::from(l < r),
                    BinOp::Gt => i32::from(l > r),
                    BinOp::Le => i32::from(l <= r),
                    BinOp::Ge => i32::from(l >= r),
                    BinOp::Shl => l.wrapping_shl((r & 0xFF) as u32),
                    BinOp::Shr => l.wrapping_shr((r & 0xFF) as u32),
                    BinOp::BitAnd => l & r,
                    BinOp::BitOr => l | r,
                    BinOp::BitXor => l ^ r,
                    BinOp::LogAnd => i32::from((l != 0) && (r != 0)),
                    BinOp::LogOr => i32::from((l != 0) || (r != 0)),
                })
            }
            Expr::Call { .. } => None,
            Expr::StrLit(_) => None,
            Expr::Global(_) => None,
            Expr::Index { .. } | Expr::IndexByte { .. } | Expr::PtrIndexByte { .. } => None,
            Expr::LocalIndex { .. } | Expr::LocalIndexByte { .. } => None,
            Expr::ParamIndex { .. } => None,
            Expr::LocalField { .. } | Expr::DerefLocalField { .. } | Expr::GlobalField { .. } => None,
            Expr::DerefParamField { .. } => None,
            Expr::DerefByte { .. } | Expr::DerefWord { .. } => None,
            Expr::AddrOfGlobal(_) | Expr::AddrOfLocal(_) => None,
            Expr::Ternary { cond, then_arm, else_arm } => {
                let c = cond.fold(locals)?;
                if c != 0 {
                    then_arm.fold(locals)
                } else {
                    else_arm.fold(locals)
                }
            }
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
    Amp,
    Semi,
    Assign,
    EqEq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
    Shl,
    Shr,
    Slash,
    Percent,
    Pipe,
    Caret,
    Tilde,
    Bang,
    Quest,
    Colon,
    Dot,
    Arrow,
    PlusPlus,
    MinusMinus,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AndEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,
    AndAnd,
    OrOr,
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
            b'&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    toks.push(Tok::AndAnd); i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::AndEq); i += 2;
                } else {
                    toks.push(Tok::Amp); i += 1;
                }
            }
            b';' => { toks.push(Tok::Semi); i += 1; }
            b',' => { toks.push(Tok::Comma); i += 1; }
            b'+' => {
                if bytes.get(i + 1) == Some(&b'+') {
                    toks.push(Tok::PlusPlus); i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::PlusEq); i += 2;
                } else {
                    toks.push(Tok::Plus); i += 1;
                }
            }
            b'*' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::StarEq); i += 2;
                } else {
                    toks.push(Tok::Star); i += 1;
                }
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::Le); i += 2;
                } else if bytes.get(i + 1) == Some(&b'<') {
                    if bytes.get(i + 2) == Some(&b'=') {
                        toks.push(Tok::ShlEq); i += 3;
                    } else {
                        toks.push(Tok::Shl); i += 2;
                    }
                } else {
                    toks.push(Tok::Lt); i += 1;
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::Ge); i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    if bytes.get(i + 2) == Some(&b'=') {
                        toks.push(Tok::ShrEq); i += 3;
                    } else {
                        toks.push(Tok::Shr); i += 2;
                    }
                } else {
                    toks.push(Tok::Gt); i += 1;
                }
            }
            b'/' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::SlashEq); i += 2;
                } else if bytes.get(i + 1) == Some(&b'/') {
                    while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
                } else if bytes.get(i + 1) == Some(&b'*') {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    if i + 1 >= bytes.len() {
                        return Err(EmitError::Unsupported(
                            "unterminated `/* ... */` comment".to_owned(),
                        ));
                    }
                    i += 2;
                } else {
                    toks.push(Tok::Slash); i += 1;
                }
            }
            b'%' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::PercentEq); i += 2;
                } else {
                    toks.push(Tok::Percent); i += 1;
                }
            }
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    toks.push(Tok::OrOr); i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::PipeEq); i += 2;
                } else {
                    toks.push(Tok::Pipe); i += 1;
                }
            }
            b'^' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::CaretEq); i += 2;
                } else {
                    toks.push(Tok::Caret); i += 1;
                }
            }
            b'~' => { toks.push(Tok::Tilde); i += 1; }
            b'?' => { toks.push(Tok::Quest); i += 1; }
            b':' => { toks.push(Tok::Colon); i += 1; }
            b'.' => { toks.push(Tok::Dot); i += 1; }
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
                    toks.push(Tok::Bang);
                    i += 1;
                }
            }
            b'-' => {
                if bytes.get(i + 1) == Some(&b'-') {
                    toks.push(Tok::MinusMinus); i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::MinusEq); i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    toks.push(Tok::Arrow); i += 2;
                } else {
                    toks.push(Tok::Minus); i += 1;
                }
            }
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
            b'\'' => {
                // Char literal — single byte (with escape support)
                // bracketed by `'`. Becomes a `Tok::Int(byte)`; the
                // C semantics widen char to int for free.
                i += 1;
                if i >= bytes.len() {
                    return Err(EmitError::Unsupported("unterminated char literal".to_owned()));
                }
                let value: i32 = if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    let esc = bytes[i + 1];
                    let v = match esc {
                        b'n' => 0x0A,
                        b't' => 0x09,
                        b'r' => 0x0D,
                        b'0' => 0x00,
                        b'\\' => b'\\',
                        b'\'' => b'\'',
                        _ => {
                            return Err(EmitError::Unsupported(format!(
                                "unknown escape `\\{}` in char literal",
                                esc as char
                            )));
                        }
                    };
                    i += 2;
                    v as i32
                } else {
                    let v = bytes[i] as i32;
                    i += 1;
                    v
                };
                if i >= bytes.len() || bytes[i] != b'\'' {
                    return Err(EmitError::Unsupported("unterminated char literal".to_owned()));
                }
                i += 1;
                toks.push(Tok::Int(value));
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
                // Hex (`0x` / `0X`), octal (`0` followed by digits),
                // and decimal forms. Trailing L/U/UL suffixes ignored.
                let n: i32 = if bytes.get(i) == Some(&b'0')
                    && matches!(bytes.get(i + 1), Some(&b'x') | Some(&b'X'))
                {
                    i += 2;
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                        i += 1;
                    }
                    let text = std::str::from_utf8(&bytes[start..i])
                        .map_err(|_| EmitError::Unsupported("non-ASCII in hex int".to_owned()))?;
                    i32::from_str_radix(text, 16)
                        .or_else(|_| u32::from_str_radix(text, 16).map(|u| u as i32))
                        .map_err(|_| EmitError::Unsupported(format!("bad hex `0x{text}`")))?
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    let text = std::str::from_utf8(&bytes[start..i])
                        .map_err(|_| EmitError::Unsupported("non-ASCII in integer".to_owned()))?;
                    if let Some(rest) = text.strip_prefix('0').filter(|s| !s.is_empty() && s.bytes().all(|b| (b'0'..=b'7').contains(&b))) {
                        i32::from_str_radix(rest, 8)
                            .map_err(|_| EmitError::Unsupported(format!("bad octal `0{rest}`")))?
                    } else {
                        text.parse()
                            .map_err(|_| EmitError::Unsupported(format!("bad integer `{text}`")))?
                    }
                };
                // Skip trailing L/U/l/u suffix bytes; we promote
                // everything to int in Phase 1.
                while matches!(bytes.get(i), Some(b'L') | Some(b'l') | Some(b'U') | Some(b'u')) {
                    i += 1;
                }
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
                    "short" => Tok::Kw("short"),
                    "main" => Tok::Kw("main"),
                    "void" => Tok::Kw("void"),
                    "return" => Tok::Kw("return"),
                    "if" => Tok::Kw("if"),
                    "else" => Tok::Kw("else"),
                    "while" => Tok::Kw("while"),
                    "do" => Tok::Kw("do"),
                    "for" => Tok::Kw("for"),
                    "switch" => Tok::Kw("switch"),
                    "case" => Tok::Kw("case"),
                    "default" => Tok::Kw("default"),
                    "break" => Tok::Kw("break"),
                    "continue" => Tok::Kw("continue"),
                    "struct" => Tok::Kw("struct"),
                    "sizeof" => Tok::Kw("sizeof"),
                    "long" => Tok::Kw("long"),
                    // Storage-class / qualifier modifiers we currently
                    // treat as no-ops in declarator parsing.
                    "unsigned" => Tok::Kw("unsigned"),
                    "signed" => Tok::Kw("signed"),
                    "static" => Tok::Kw("static"),
                    "extern" => Tok::Kw("extern"),
                    "register" => Tok::Kw("register"),
                    "auto" => Tok::Kw("auto"),
                    "volatile" => Tok::Kw("volatile"),
                    "const" => Tok::Kw("const"),
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
    /// Mirror of `local_names`, populated as locals are pushed in
    /// `parse_function`. Lets parse_atom / parse_stmt look up a
    /// local's array_len + size to pick the right Index variant
    /// without needing to thread `&[LocalSpec]` through every call.
    local_specs: Vec<LocalSpec>,
    param_names: Vec<String>,
    /// For each param, `Some(struct_idx)` when its type is
    /// `struct S *` (or `struct S` array-decayed). Used by parse_atom
    /// to resolve `<param>-><field>` lookups.
    param_struct_idxs: Vec<Option<usize>>,
    /// File-scope global names in source order; the index doubles
    /// as the `Expr::Global(idx)` value.
    global_names: Vec<String>,
    /// Same source order, used to materialize the `Unit::globals`.
    globals: Vec<Global>,
    /// Named struct definitions collected at parse time, by source
    /// order. The position in the Vec is the `struct_idx` referenced
    /// by `LocalSpec::struct_idx` and `Global::struct_idx`.
    structs: Vec<StructDef>,
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
        local_specs: Vec::new(),
        param_names: Vec::new(),
        param_struct_idxs: Vec::new(),
        global_names: Vec::new(),
        globals: Vec::new(),
        structs: Vec::new(),
        strings: Vec::new(),
    };
    let mut functions = Vec::new();
    let mut decl_order: Vec<TopDecl> = Vec::new();
    while p.peek().is_some() {
        // Skip any preprocessor directives at file scope.
        if matches!(p.peek(), Some(Tok::PreprocLine)) {
            p.bump();
            continue;
        }
        // `struct <Name> { ... };` — record the struct definition.
        // `struct <Name> <var>;` and `struct <Name> *<var>;` fall
        // into the global-decl path further down.
        if matches!(p.peek(), Some(Tok::Kw("struct")))
            && matches!(p.toks.get(p.pos + 1), Some(Tok::Ident(_)))
            && matches!(p.toks.get(p.pos + 2), Some(Tok::LBrace))
        {
            parse_struct_def(&mut p)?;
            continue;
        }
        // Disambiguate file-scope `int <name>...;` (global) from
        // `int <name>(...) { ... }` (function) by looking ahead
        // across any leading modifier keywords. A `(` after the
        // identifier means function; everything else means decl.
        let mut k = p.pos;
        while matches!(
            p.toks.get(k),
            Some(Tok::Kw("unsigned")) | Some(Tok::Kw("signed"))
                | Some(Tok::Kw("static")) | Some(Tok::Kw("extern"))
                | Some(Tok::Kw("register")) | Some(Tok::Kw("auto"))
                | Some(Tok::Kw("volatile")) | Some(Tok::Kw("const"))
                | Some(Tok::Kw("short"))
        ) {
            k += 1;
        }
        let is_type_prefix = matches!(
            p.toks.get(k),
            Some(Tok::Kw("int")) | Some(Tok::Kw("char")) | Some(Tok::Kw("long"))
        ) || (k > p.pos && matches!(p.toks.get(k), Some(Tok::Ident(_))))
            || matches!(p.toks.get(k), Some(Tok::Kw("struct")));
        if is_type_prefix {
            // Walk past the type kw (plus the struct's name token if
            // it's a `struct <Name>` prefix) + optional `*` to look
            // at the declarator's first token after the name.
            let mut after = k + 1;
            if matches!(p.toks.get(k), Some(Tok::Kw("struct"))) {
                after += 1; // consume the struct's name
            }
            if matches!(p.toks.get(after), Some(Tok::Star)) { after += 1; }
            // Now expect an ident or the `main` keyword. The token
            // after the name decides function (`(`) vs global decl.
            let name_ok = matches!(
                p.toks.get(after),
                Some(Tok::Ident(_)) | Some(Tok::Kw("main"))
            );
            let is_function = name_ok
                && matches!(p.toks.get(after + 1), Some(Tok::LParen));
            if !is_function {
                let before = p.globals.len();
                parse_global_decl(&mut p)?;
                for i in before..p.globals.len() {
                    decl_order.push(TopDecl::Global(i));
                }
                continue;
            }
        }
        let fn_idx = functions.len();
        functions.push(parse_function(&mut p)?);
        decl_order.push(TopDecl::Function(fn_idx));
    }
    if functions.is_empty() {
        return Err(EmitError::Unsupported(
            "translation unit has no functions".to_owned(),
        ));
    }
    Ok(Unit { globals: p.globals, structs: p.structs, functions, decl_order, strings: p.strings })
}

/// Parse a file-scope `struct <Name> <var> [= { ... }];` declaration.
/// Stores the struct global as if it were a `char` array sized to
/// the struct's total_bytes — that gives correct storage layout
/// without needing a separate Global::struct_idx field. Initializer
/// values are mapped to per-field byte slots.
fn parse_struct_global_decl(p: &mut Parser<'_>) -> Result<(), EmitError> {
    p.eat(&Tok::Kw("struct"))?;
    let sname = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected struct name in global decl, got {other:?}"
            )));
        }
    };
    let sidx = p.structs.iter().position(|s| s.name == sname).ok_or_else(|| {
        EmitError::Unsupported(format!("unknown struct `{sname}` in global decl"))
    })?;
    let stotal = p.structs[sidx].total_bytes;
    let is_pointer = matches!(p.peek(), Some(Tok::Star));
    if is_pointer { p.bump(); }
    let name = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected global name, got {other:?}"
            )));
        }
    };
    let init = if matches!(p.peek(), Some(Tok::Assign)) {
        p.bump();
        if !is_pointer && matches!(p.peek(), Some(Tok::LBrace)) {
            p.bump();
            let mut slots: Vec<GlobalInit> = Vec::new();
            let mut field_idx = 0usize;
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                let v = parse_signed_int(p)?;
                let field = &p.structs[sidx].fields[field_idx];
                while slots.len() < field.byte_off as usize {
                    slots.push(GlobalInit::Byte(0));
                }
                if field.size == 1 {
                    slots.push(GlobalInit::Byte((v as u32 & 0xFF) as u8));
                } else {
                    slots.push(GlobalInit::Int(v));
                }
                field_idx += 1;
                if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
            }
            p.eat(&Tok::RBrace)?;
            while slots.iter().map(GlobalInit::size_bytes).sum::<usize>() < stotal {
                slots.push(GlobalInit::Byte(0));
            }
            Some(slots)
        } else if is_pointer && matches!(p.peek(), Some(Tok::Amp)) {
            p.bump();
            let target_name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after `&` in init, got {other:?}"
                    )));
                }
            };
            let target_idx = p.global_names.iter().position(|n| *n == target_name)
                .ok_or_else(|| EmitError::Unsupported(format!(
                    "address-of unknown global `{target_name}`"
                )))?;
            Some(vec![GlobalInit::GlobalAddr(target_idx)])
        } else {
            return Err(EmitError::Unsupported(format!(
                "unsupported struct global init: {:?}", p.peek()
            )));
        }
    } else {
        None
    };
    p.eat(&Tok::Semi)?;
    let array_len = if is_pointer { 1 } else { stotal };
    let element_size = 1; // byte-oriented storage; fields by offset
    p.global_names.push(name.clone());
    p.globals.push(Global {
        name,
        init,
        array_len,
        element_size,
        is_pointer,
        struct_idx: Some(sidx),
        is_long: false,
    });
    Ok(())
}

/// Parse `struct Name { <field-decl>; ... };` — record the struct's
/// fields and their byte offsets. C89 padding rule: each field
/// starts at its natural alignment (even for `int`/pointer, byte
/// for `char`). MSC's small-model size is the sum of field sizes
/// without trailing pad until the next int boundary; we use the
/// same rule. Anonymous structs and bitfields aren't supported.
fn parse_struct_def(p: &mut Parser<'_>) -> Result<(), EmitError> {
    p.eat(&Tok::Kw("struct"))?;
    let sname = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected struct name, got {other:?}"
            )));
        }
    };
    p.eat(&Tok::LBrace)?;
    let mut fields: Vec<StructField> = Vec::new();
    let mut cursor: usize = 0;
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        skip_decl_modifiers(p);
        let size: u8 = match p.bump().cloned() {
            Some(Tok::Kw("int")) => 2,
            Some(Tok::Kw("char")) => 1,
            other => {
                return Err(EmitError::Unsupported(format!(
                    "struct field type not yet supported: {other:?}"
                )));
            }
        };
        let is_ptr = if matches!(p.peek(), Some(Tok::Star)) {
            p.bump();
            true
        } else {
            false
        };
        let fname = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected struct field name, got {other:?}"
                )));
            }
        };
        let field_size = if is_ptr { 2 } else { size };
        // Word-align int / pointer fields. Char fields take the
        // next byte at any offset.
        if field_size == 2 && cursor % 2 != 0 {
            cursor += 1;
        }
        let byte_off = u16::try_from(cursor).expect("field offset fits in u16");
        fields.push(StructField {
            name: fname,
            byte_off,
            size: field_size,
        });
        cursor += field_size as usize;
        p.eat(&Tok::Semi)?;
    }
    p.eat(&Tok::RBrace)?;
    p.eat(&Tok::Semi)?;
    // Round total up to the natural alignment (2 bytes for any
    // struct containing an int/pointer field; 1 byte otherwise).
    let needs_word_align = fields.iter().any(|f| f.size == 2);
    let total_bytes = if needs_word_align { (cursor + 1) & !1 } else { cursor };
    p.structs.push(StructDef { name: sname, fields, total_bytes });
    Ok(())
}

/// Parse one file-scope `<type> <name> [= <init>];` declaration and
/// register it in the parser's globals list. Phase 1 covers
/// `int <name>`, `int <name>[N]`, and `char *<name>` with optional
/// initializer. Caller has confirmed the next tokens form a
/// declaration, not a function.
fn parse_global_decl(p: &mut Parser<'_>) -> Result<(), EmitError> {
    // Skip any leading storage/qualifier modifiers (unsigned, static,
    // ...) — we treat them all as no-ops at the codegen level.
    let mods_consumed = skip_decl_modifiers(p);
    // `struct <Name> name [= {...}] ;` and `struct <Name> *name [= ...] ;`
    // routed through a separate parse path because the size + element
    // model differ from primitive types.
    if matches!(p.peek(), Some(Tok::Kw("struct"))) {
        return parse_struct_global_decl(p);
    }
    // Type prefix. Phase 1 globals: `int [*]`, `char *`, `char [N]`,
    // and minimal `long` support (storage only; arithmetic not yet).
    // Bare `unsigned x;` (no following int/char) implies int.
    let mut is_pointer = false;
    let mut is_char = false;
    let mut is_long = false;
    match p.peek() {
        Some(Tok::Kw("int")) => {
            p.bump();
            if matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
                is_pointer = true;
            }
        }
        Some(Tok::Kw("char")) => {
            p.bump();
            is_char = true;
            if matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
                is_pointer = true;
            }
        }
        Some(Tok::Kw("long")) => {
            p.bump();
            // `long int` is just `long`.
            if matches!(p.peek(), Some(Tok::Kw("int"))) { p.bump(); }
            is_long = true;
        }
        // Bare modifier (`unsigned x;`) → implicit int.
        Some(Tok::Ident(_)) if mods_consumed > 0 => {}
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected `int`, `int *`, `long`, `char *`, or `char [...]` for global, got {other:?}"
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
        } else if is_char && matches!(p.peek(), Some(Tok::StrLit(_))) {
            // `char a[N] = "...";` — bytes land directly in _DATA.
            // Trailing NUL is included; if the literal is shorter than
            // N, the remainder stays zero-filled by the linker.
            let bytes = match p.bump().cloned() {
                Some(Tok::StrLit(b)) => b,
                _ => unreachable!(),
            };
            let mut slots: Vec<GlobalInit> =
                bytes.iter().map(|b| GlobalInit::Byte(*b)).collect();
            // C semantics: include the implicit NUL if it fits.
            if slots.len() < array_len {
                slots.push(GlobalInit::Byte(0));
            }
            Some(slots)
        } else if is_pointer && matches!(p.peek(), Some(Tok::Amp)) {
            p.bump();
            let target_name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after `&` in initializer, got {other:?}"
                    )));
                }
            };
            let target_idx = p.global_names.iter().position(|n| *n == target_name)
                .ok_or_else(|| EmitError::Unsupported(format!(
                    "address-of unknown global `{target_name}`"
                )))?;
            Some(vec![GlobalInit::GlobalAddr(target_idx)])
        } else if is_long {
            let k = parse_signed_int(p)?;
            let low = (k as u32 & 0xFFFF) as i32;
            let high = (((k as u32) >> 16) & 0xFFFF) as i32;
            Some(vec![GlobalInit::Int(low), GlobalInit::Int(high)])
        } else {
            Some(vec![GlobalInit::Int(parse_signed_int(p)?)])
        }
    } else {
        None
    };
    p.eat(&Tok::Semi)?;
    // `element_size` describes the pointed-to or array-element type
    // (1 for `char` family, 2 otherwise). `is_pointer` is set when
    // the declarator carries a `*`. Storage size is independent:
    // pointers are always 2 bytes; arrays scale by `array_len`.
    let element_size = if is_char { 1 } else { 2 };
    // Long storage is 4 bytes; modeled as a 2-slot word array. Reads of
    // `(int)g` naturally pick up the low word at the base address.
    let (array_len, element_size) = if is_long && !is_pointer {
        (2usize, 2usize)
    } else {
        (array_len, element_size)
    };
    p.global_names.push(name.clone());
    p.globals.push(Global { name, init, array_len, element_size, is_pointer, struct_idx: None, is_long });
    Ok(())
}

fn parse_function(p: &mut Parser<'_>) -> Result<Function, EmitError> {
    // `<modifiers>* <ret-type> <name>(...)` — skip any leading
    // storage-class / sign keywords (static, extern, unsigned, etc.),
    // then expect `int` / `char` / `void`. `char` returns are widened
    // to int via cbw at the consume site; treat as int here.
    skip_decl_modifiers(p);
    let return_int = match p.bump().cloned() {
        Some(Tok::Kw("int")) => true,
        Some(Tok::Kw("char")) => true,
        Some(Tok::Kw("void")) => false,
        Some(Tok::Kw("struct")) => {
            // Skip the struct's name. Phase 1 only models functions
            // that return a struct *pointer* (`struct S *f()`) — full
            // struct-by-value returns need a hidden-pointer ABI we
            // don't support yet.
            if matches!(p.peek(), Some(Tok::Ident(_))) { p.bump(); }
            true
        }
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
        (Vec::new(), Vec::new())
    } else {
        let mut names = Vec::new();
        let mut struct_idxs: Vec<Option<usize>> = Vec::new();
        loop {
            // Optional sign/qualifier modifiers, then `int` / `char` /
            // `struct Name`. Pointers (`<type> *<name>`) consume one
            // stack slot regardless of pointee type.
            skip_decl_modifiers(p);
            let mut struct_idx: Option<usize> = None;
            match p.peek() {
                Some(Tok::Kw("int")) | Some(Tok::Kw("char")) => { p.bump(); }
                Some(Tok::Kw("struct")) => {
                    p.bump();
                    let sname = match p.bump().cloned() {
                        Some(Tok::Ident(s)) => s,
                        other => {
                            return Err(EmitError::Unsupported(format!(
                                "expected struct name in param, got {other:?}"
                            )));
                        }
                    };
                    struct_idx = p.structs.iter().position(|s| s.name == sname);
                }
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected `int`, `char`, or `struct` in parameter type, got {other:?}"
                    )));
                }
            }
            if matches!(p.peek(), Some(Tok::Star)) {
                p.bump();
            }
            let pname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected parameter name, got {other:?}"
                    )));
                }
            };
            // `int a[]` and `int a[N]` decay to `int *a`. Eat the
            // optional bracket pair.
            if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                while !matches!(p.peek(), Some(Tok::RBrack)) {
                    p.bump();
                }
                p.eat(&Tok::RBrack)?;
            }
            names.push(pname);
            struct_idxs.push(struct_idx);
            if matches!(p.peek(), Some(Tok::Comma)) {
                p.bump();
                continue;
            }
            break;
        }
        (names, struct_idxs)
    };
    let (params, param_struct_idxs) = match params {
        x => x,
    };
    p.eat(&Tok::RParen)?;
    p.eat(&Tok::LBrace)?;

    // Reset per-function name lists, then populate with this
    // function's params before parsing the body.
    p.local_names.clear();
    p.local_specs.clear();
    p.param_names = params.clone();
    p.param_struct_idxs = param_struct_idxs;

    // `[storage-class]+ int|char <name> [= <init>] (, <name> [= <init>])* ;`
    //
    // A non-constant init becomes a synthetic assignment statement
    // prepended to the body.
    let mut locals: Vec<LocalSpec> = Vec::new();
    let mut prelude: Vec<Stmt> = Vec::new();
    loop {
        // Peek across leading modifier keywords. The decl is a local
        // when the next token is int/char OR when *any* modifier was
        // present (bare `unsigned x;` means `unsigned int x;`).
        let mut peek_pos = p.pos;
        let start_pos = peek_pos;
        while matches!(
            p.toks.get(peek_pos),
            Some(Tok::Kw("unsigned")) | Some(Tok::Kw("signed"))
                | Some(Tok::Kw("static")) | Some(Tok::Kw("extern"))
                | Some(Tok::Kw("register")) | Some(Tok::Kw("auto"))
                | Some(Tok::Kw("volatile")) | Some(Tok::Kw("const"))
                | Some(Tok::Kw("short"))
        ) {
            peek_pos += 1;
        }
        // `struct <Name>` form is a separate path because the size is
        // looked up from the struct registry rather than a primitive
        // type token. Each declarator can still be `s` (struct value)
        // or `*s` (struct pointer).
        if matches!(p.toks.get(peek_pos), Some(Tok::Kw("struct"))) {
            skip_decl_modifiers(p);
            p.bump(); // struct
            let sname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected struct name in local decl, got {other:?}"
                    )));
                }
            };
            let sidx = p.structs.iter().position(|s| s.name == sname).ok_or_else(|| {
                EmitError::Unsupported(format!("unknown struct `{sname}` in local decl"))
            })?;
            let stotal = p.structs[sidx].total_bytes;
            loop {
                let is_ptr = if matches!(p.peek(), Some(Tok::Star)) {
                    p.bump();
                    true
                } else {
                    false
                };
                let lname = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected identifier in struct decl, got {other:?}"
                        )));
                    }
                };
                let spec = if is_ptr {
                    LocalSpec { size: 2, array_len: 1, init: None, struct_idx: Some(sidx), is_long: false, init_is_literal: false }
                } else {
                    LocalSpec { size: 1, array_len: stotal, init: None, struct_idx: Some(sidx), is_long: false, init_is_literal: false }
                };
                p.local_names.push(lname);
                p.local_specs.push(spec.clone());
                locals.push(spec);
                if matches!(p.peek(), Some(Tok::Comma)) {
                    p.bump();
                    continue;
                }
                break;
            }
            p.eat(&Tok::Semi)?;
            continue;
        }
        let (size, has_explicit_type, is_long_decl) = match p.toks.get(peek_pos) {
            Some(Tok::Kw("int")) => (2usize, true, false),
            Some(Tok::Kw("char")) => (1usize, true, false),
            Some(Tok::Kw("long")) => (2usize, true, true),
            // `unsigned x;` / `signed x;` → implicit int.
            _ if peek_pos > start_pos
                && matches!(p.toks.get(peek_pos), Some(Tok::Ident(_))) =>
            {
                (2usize, false, false)
            }
            _ => break,
        };
        skip_decl_modifiers(p);
        if has_explicit_type {
            p.bump(); // type kw
            // Consume optional `int` after `long` (i.e. `long int`).
            if is_long_decl && matches!(p.peek(), Some(Tok::Kw("int"))) {
                p.bump();
            }
        }
        loop {
            // Per-declarator `*` prefix for pointer locals.
            while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
            let lname = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier in declaration, got {other:?}"
                    )));
                }
            };
            // Optional `[N]` for an array decl.
            let array_len = if matches!(p.peek(), Some(Tok::LBrack)) {
                p.bump();
                let n = match p.bump().cloned() {
                    Some(Tok::Int(k)) if k > 0 => k as usize,
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected positive array length for local, got {other:?}"
                        )));
                    }
                };
                p.eat(&Tok::RBrack)?;
                n
            } else {
                1
            };
            let local_idx = locals.len();
            p.local_names.push(lname);
            // Long: 4-byte slot modeled as a 2-word "array". Reads via
            // `Expr::Local(idx)` pick up the low word at [bp-disp].
            let (slot_size, slot_len, is_long_slot) = if is_long_decl && array_len == 1 {
                (2usize, 2usize, true)
            } else {
                (size, array_len, false)
            };
            let spec = LocalSpec { size: slot_size, array_len: slot_len, init: None, struct_idx: None, is_long: is_long_slot, init_is_literal: false };
            p.local_specs.push(spec.clone());
            locals.push(spec);
            if matches!(p.peek(), Some(Tok::Assign)) {
                p.bump();
                if array_len > 1 && matches!(p.peek(), Some(Tok::LBrace)) {
                    // `int a[N] = { v0, v1, ... };` — synthesize an
                    // a[i] = vi store for each element.
                    p.bump();
                    let mut i = 0usize;
                    while !matches!(p.peek(), Some(Tok::RBrace)) {
                        let value = parse_expr(p)?;
                        let byte_off = u16::try_from(i * size)
                            .expect("brace-init byte_off fits");
                        prelude.push(Stmt::Assign {
                            target: if size == 1 {
                                AssignTarget::IndexedLocalByte { local: local_idx, byte_off }
                            } else {
                                AssignTarget::IndexedLocal { local: local_idx, byte_off }
                            },
                            value,
                        });
                        i += 1;
                        if matches!(p.peek(), Some(Tok::Comma)) { p.bump(); }
                    }
                    p.eat(&Tok::RBrace)?;
                } else if array_len > 1 && size == 1 && matches!(p.peek(), Some(Tok::StrLit(_))) {
                    // `char s[N] = "literal";` — break into per-byte
                    // stores so const-prop can see each element.
                    let bytes = match p.bump().cloned() {
                        Some(Tok::StrLit(b)) => b,
                        _ => unreachable!(),
                    };
                    let mut with_nul = bytes.clone();
                    with_nul.push(0);
                    for (i, b) in with_nul.iter().enumerate().take(array_len) {
                        prelude.push(Stmt::Assign {
                            target: AssignTarget::IndexedLocalByte {
                                local: local_idx,
                                byte_off: u16::try_from(i).expect("byte_off fits"),
                            },
                            value: Expr::IntLit(*b as i32),
                        });
                    }
                } else {
                    let init_expr = parse_expr(p)?;
                    let init_view: Vec<Option<i32>> = locals
                        .iter()
                        .take(local_idx)
                        .map(|l| l.init)
                        .collect();
                    if let Some(k) = init_expr.fold(&init_view) {
                        locals[local_idx].init = Some(k);
                        // Pure literal init: folds without any local
                        // references. Charac locals use this to decide
                        // whether `return c;` re-reads through the slot
                        // (fixture 1023 vs 1046).
                        let pure = init_expr.fold(&[]).is_some();
                        locals[local_idx].init_is_literal = pure;
                    } else {
                        prelude.push(Stmt::Assign {
                            target: AssignTarget::Local(local_idx),
                            value: init_expr,
                        });
                    }
                }
            }
            if matches!(p.peek(), Some(Tok::Comma)) {
                p.bump();
                continue;
            }
            break;
        }
        p.eat(&Tok::Semi)?;
    }

    // Body statements until the closing `}`. Any synthetic assigns
    // from non-constant local inits run first.
    let mut body = prelude;
    while !matches!(p.peek(), Some(Tok::RBrace)) {
        body.push(parse_stmt(p)?);
    }
    p.eat(&Tok::RBrace)?;

    Ok(Function { name, return_int, params, locals, body })
}

fn parse_signed_int(p: &mut Parser<'_>) -> Result<i32, EmitError> {
    // Accept any compile-time constant expression — integer literal,
    // negated literal, or any binop chain that folds to a constant.
    // Used for global inits, brace inits, case labels, array sizes.
    let expr = parse_expr(p)?;
    expr.fold(&[]).ok_or_else(|| EmitError::Unsupported(
        format!("expected constant expression in init, got {expr:?}")
    ))
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
        Some(Tok::Kw("switch")) => {
            p.bump();
            p.eat(&Tok::LParen)?;
            let scrutinee = parse_expr(p)?;
            p.eat(&Tok::RParen)?;
            p.eat(&Tok::LBrace)?;
            let mut cases: Vec<SwitchArm> = Vec::new();
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                let value = match p.peek() {
                    Some(Tok::Kw("case")) => {
                        p.bump();
                        Some(parse_signed_int(p)?)
                    }
                    Some(Tok::Kw("default")) => {
                        p.bump();
                        None
                    }
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected `case` or `default` in switch, got {other:?}"
                        )));
                    }
                };
                p.eat(&Tok::Colon)?;
                let mut body = Vec::new();
                while !matches!(
                    p.peek(),
                    Some(Tok::Kw("case")) | Some(Tok::Kw("default")) | Some(Tok::RBrace)
                ) {
                    body.push(parse_stmt(p)?);
                }
                cases.push(SwitchArm { value, body });
            }
            p.eat(&Tok::RBrace)?;
            Ok(Stmt::Switch { scrutinee, cases })
        }
        Some(Tok::Kw("break")) => {
            p.bump();
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Break)
        }
        Some(Tok::Kw("continue")) => {
            p.bump();
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Continue)
        }
        Some(Tok::Semi) => {
            p.bump();
            Ok(Stmt::Empty)
        }
        Some(Tok::LBrace) => {
            // Block statement: `{ <stmt>* }`. Lowered as a Block
            // variant so caller (if/while/etc.) can treat it as one
            // statement.
            p.bump();
            let mut stmts = Vec::new();
            while !matches!(p.peek(), Some(Tok::RBrace)) {
                stmts.push(parse_stmt(p)?);
            }
            p.eat(&Tok::RBrace)?;
            Ok(Stmt::Block(stmts))
        }
        Some(Tok::Star) => {
            // `*<ident> = <expr>;` — store through a pointer.
            // The pointer can be a global or a local.
            p.bump();
            let target_name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after `*`, got {other:?}"
                    )));
                }
            };
            let target = if let Some(idx) = p.local_names.iter().position(|n| *n == target_name) {
                AssignTarget::DerefLocal(idx)
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == target_name) {
                AssignTarget::DerefGlobal(idx)
            } else {
                return Err(EmitError::Unsupported(format!(
                    "deref-store through unknown identifier `{target_name}`"
                )));
            };
            p.eat(&Tok::Assign)?;
            let value = parse_expr(p)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Assign { target, value })
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
            // `<struct-global>.<field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Dot))
                && let Some(global_idx) = p.global_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.globals[global_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::GlobalField { global: global_idx, byte_off, size },
                    value,
                });
            }
            // `<struct-local>.<field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Dot))
                && let Some(local_idx) = p.local_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.local_specs[local_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::LocalField { local: local_idx, byte_off, size },
                    value,
                });
            }
            // `<struct-ptr-param>-><field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Arrow))
                && let Some(param_idx) = p.param_names.iter().position(|n| *n == name)
                && let Some(Some(sidx)) = p.param_struct_idxs.get(param_idx).cloned()
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                if let Some(value) = parse_compound_rhs_for_indexed(p, param_idx, byte_off, size == 1, false)? {
                    // Compound assign — fall through using AssignTarget::DerefParamField with the rewritten RHS.
                    p.eat(&Tok::Semi)?;
                    return Ok(Stmt::Assign {
                        target: AssignTarget::DerefParamField { ptr_param: param_idx, byte_off, size },
                        value,
                    });
                }
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::DerefParamField { ptr_param: param_idx, byte_off, size },
                    value,
                });
            }
            // `<struct-ptr-local>-><field> = <expr>;`
            if matches!(p.peek(), Some(Tok::Arrow))
                && let Some(local_idx) = p.local_names.iter().position(|n| *n == name)
                && let Some(sidx) = p.local_specs[local_idx].struct_idx
            {
                p.bump();
                let (byte_off, size) = parse_field_lookup(p, sidx)?;
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign {
                    target: AssignTarget::DerefLocalField { ptr_local: local_idx, byte_off, size },
                    value,
                });
            }
            // `<local-array>[K] = <expr>;` (and compound shapes:
            // `+=`, `-=`, `*=`, `++`, `--`, etc.) — indexed local
            // array store.
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Some(local_idx) = p.local_names.iter().position(|n| *n == name)
            {
                p.bump(); // [
                let index_expr = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                let k = index_expr.fold(&[]).ok_or_else(|| EmitError::Unsupported(
                    "non-constant local-array index in store not yet supported".to_owned(),
                ))?;
                let elem_bytes = p.local_specs[local_idx].size;
                let byte_off = u16::try_from((k as i64) * (elem_bytes as i64))
                    .expect("indexed-store byte offset fits");
                let target = if elem_bytes == 1 {
                    AssignTarget::IndexedLocalByte { local: local_idx, byte_off }
                } else {
                    AssignTarget::IndexedLocal { local: local_idx, byte_off }
                };
                let value = if let Some(v) = parse_compound_rhs_for_indexed(
                    p, local_idx, byte_off, elem_bytes == 1, false,
                )? {
                    v
                } else {
                    p.eat(&Tok::Assign)?;
                    parse_expr(p)?
                };
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            // `<global>[K] = <expr>;` — indexed array store.
            if matches!(p.peek(), Some(Tok::LBrack))
                && let Some(array_idx) = p.global_names.iter().position(|n| *n == name)
            {
                p.bump(); // [
                let index_expr = parse_expr(p)?;
                p.eat(&Tok::RBrack)?;
                p.eat(&Tok::Assign)?;
                let value = parse_expr(p)?;
                p.eat(&Tok::Semi)?;
                let k = index_expr.fold(&[]).ok_or_else(|| EmitError::Unsupported(
                    "non-constant array index in store not yet supported".to_owned(),
                ))?;
                let g = &p.globals[array_idx];
                let target = if g.is_pointer {
                    // `<ptr>[K] = ...` — load pointer then store at
                    // offset. Phase 1 covers the `char *p` byte form.
                    let disp = i8::try_from(k).expect("ptr index fits in i8");
                    AssignTarget::PtrIndexByte { ptr: array_idx, disp }
                } else {
                    let elem_bytes = g.element_size;
                    let byte_off = u16::try_from((k as i64) * (elem_bytes as i64))
                        .expect("indexed-store byte offset fits");
                    if elem_bytes == 1 {
                        AssignTarget::IndexedGlobalByte { array: array_idx, byte_off }
                    } else {
                        AssignTarget::IndexedGlobal { array: array_idx, byte_off }
                    }
                };
                return Ok(Stmt::Assign { target, value });
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
            // Compound forms: `x++`, `x--`, `x += K`, `x -= K`, ...
            // All rewrite to `Stmt::Assign { target, value: <existing target> <op> <rhs> }`.
            // The existing local/global codegen + peephole take it from there.
            if let Some(value) = parse_compound_rhs(p, &target)? {
                p.eat(&Tok::Semi)?;
                return Ok(Stmt::Assign { target, value });
            }
            p.eat(&Tok::Assign)?;
            let value = parse_expr(p)?;
            p.eat(&Tok::Semi)?;
            Ok(Stmt::Assign { target, value })
        }
        Some(Tok::PlusPlus) | Some(Tok::MinusMinus) => {
            // `++<ident>;` / `--<ident>;` statement — equivalent to
            // `<ident>++;` / `<ident>--;` at the codegen level.
            let inc = matches!(p.peek(), Some(Tok::PlusPlus));
            p.bump();
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after prefix `++/--`, got {other:?}"
                    )));
                }
            };
            let target = if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                AssignTarget::Local(idx)
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                AssignTarget::Global(idx)
            } else {
                return Err(EmitError::Unsupported(format!(
                    "prefix `++/--` of unknown identifier `{name}`"
                )));
            };
            p.eat(&Tok::Semi)?;
            let lvalue = match target {
                AssignTarget::Local(i) => Expr::Local(i),
                AssignTarget::Global(g) => Expr::Global(g),
                _ => unreachable!(),
            };
            Ok(Stmt::Assign {
                target,
                value: Expr::BinOp {
                    op: if inc { BinOp::Add } else { BinOp::Sub },
                    left: Box::new(lvalue),
                    right: Box::new(Expr::IntLit(1)),
                },
            })
        }
        other => Err(EmitError::Unsupported(format!(
            "statement starting with {other:?} not yet supported"
        ))),
    }
}

/// Consume any leading type-qualifier / storage-class keywords that
/// our front-end currently treats as no-ops (`unsigned`, `signed`,
/// `static`, `extern`, `register`, `auto`, `volatile`, `const`,
/// `short`). Returns the count consumed so the caller can decide
/// whether a type prefix was present.
fn skip_decl_modifiers(p: &mut Parser<'_>) -> usize {
    let mut count = 0;
    while matches!(
        p.peek(),
        Some(Tok::Kw("unsigned"))
            | Some(Tok::Kw("signed"))
            | Some(Tok::Kw("static"))
            | Some(Tok::Kw("extern"))
            | Some(Tok::Kw("register"))
            | Some(Tok::Kw("auto"))
            | Some(Tok::Kw("volatile"))
            | Some(Tok::Kw("const"))
            | Some(Tok::Kw("short"))
    ) {
        p.bump();
        count += 1;
    }
    count
}

/// Variant of `parse_compound_rhs` for indexed-array stores like
/// `a[K] += V` and `a[K]++`. The lvalue is reconstructed as
/// `Expr::Index{,Byte}` (global) or `Expr::LocalIndex{,Byte}` (local)
/// so the rewritten expression `a[K] op V` lowers through the
/// existing emit_binop path.
fn parse_compound_rhs_for_indexed(
    p: &mut Parser<'_>,
    container_idx: usize,
    byte_off: u16,
    is_byte: bool,
    is_global: bool,
) -> Result<Option<Expr>, EmitError> {
    let op = match p.peek() {
        Some(Tok::PlusPlus) => { p.bump(); BinOp::Add }
        Some(Tok::MinusMinus) => { p.bump(); BinOp::Sub }
        Some(Tok::PlusEq) => { p.bump(); BinOp::Add }
        Some(Tok::MinusEq) => { p.bump(); BinOp::Sub }
        Some(Tok::StarEq) => { p.bump(); BinOp::Mul }
        Some(Tok::SlashEq) => { p.bump(); BinOp::Div }
        Some(Tok::PercentEq) => { p.bump(); BinOp::Mod }
        Some(Tok::AndEq) => { p.bump(); BinOp::BitAnd }
        Some(Tok::PipeEq) => { p.bump(); BinOp::BitOr }
        Some(Tok::CaretEq) => { p.bump(); BinOp::BitXor }
        Some(Tok::ShlEq) => { p.bump(); BinOp::Shl }
        Some(Tok::ShrEq) => { p.bump(); BinOp::Shr }
        _ => return Ok(None),
    };
    let rhs = match op {
        BinOp::Add | BinOp::Sub
            if matches!(p.peek(), Some(Tok::Semi) | Some(Tok::Comma) | Some(Tok::RParen)) =>
        {
            Expr::IntLit(1)
        }
        _ => parse_expr(p)?,
    };
    let elem_size = if is_byte { 1 } else { 2 };
    let k = (byte_off as i64) / (elem_size as i64);
    let index = Box::new(Expr::IntLit(k as i32));
    let lvalue = if is_global {
        if is_byte {
            Expr::IndexByte { array: container_idx, index }
        } else {
            Expr::Index { array: container_idx, index }
        }
    } else if is_byte {
        Expr::LocalIndexByte { local: container_idx, index }
    } else {
        Expr::LocalIndex { local: container_idx, index }
    };
    Ok(Some(Expr::BinOp { op, left: Box::new(lvalue), right: Box::new(rhs) }))
}

/// Peek and parse a compound-assignment / post-(inc|dec) RHS for an
/// already-extracted target. Returns `Some(value)` for any compound
/// form, or `None` if the next token is just a plain `=` (the caller
/// then handles the normal `target = expr;` path). Each compound
/// form rewrites to an equivalent `Expr::BinOp(<lvalue>, <op>, rhs)`
/// so the existing target-store codegen + `x = x ± 1 → inc/dec`
/// peephole kick in for free.
fn parse_compound_rhs(p: &mut Parser<'_>, target: &AssignTarget) -> Result<Option<Expr>, EmitError> {
    let lvalue_expr = match target {
        AssignTarget::Local(i) => Expr::Local(*i),
        AssignTarget::Global(g) => Expr::Global(*g),
        _ => return Ok(None),
    };
    let op = match p.peek() {
        Some(Tok::PlusPlus) => { p.bump(); BinOp::Add }
        Some(Tok::MinusMinus) => { p.bump(); BinOp::Sub }
        Some(Tok::PlusEq) => { p.bump(); BinOp::Add }
        Some(Tok::MinusEq) => { p.bump(); BinOp::Sub }
        Some(Tok::StarEq) => { p.bump(); BinOp::Mul }
        Some(Tok::SlashEq) => { p.bump(); BinOp::Div }
        Some(Tok::PercentEq) => { p.bump(); BinOp::Mod }
        Some(Tok::AndEq) => { p.bump(); BinOp::BitAnd }
        Some(Tok::PipeEq) => { p.bump(); BinOp::BitOr }
        Some(Tok::CaretEq) => { p.bump(); BinOp::BitXor }
        Some(Tok::ShlEq) => { p.bump(); BinOp::Shl }
        Some(Tok::ShrEq) => { p.bump(); BinOp::Shr }
        _ => return Ok(None),
    };
    let rhs = match op {
        BinOp::Add | BinOp::Sub
            if matches!(p.peek(), Some(Tok::Semi) | Some(Tok::Comma) | Some(Tok::RParen)) =>
        {
            // Post-(in|de)crement form: `x++;` / `x--;`. RHS is implicit 1.
            Expr::IntLit(1)
        }
        _ => parse_expr(p)?,
    };
    Ok(Some(Expr::BinOp {
        op,
        left: Box::new(lvalue_expr),
        right: Box::new(rhs),
    }))
}

/// Parse `<local> = <expr>` (no trailing `;`) — used inside
/// for-clauses where the semis are the for-syntax separators, not
/// statement terminators.
fn parse_assign_no_semi(p: &mut Parser<'_>) -> Result<Stmt, EmitError> {
    // Prefix `++<ident>` / `--<ident>` in the for-step position.
    if matches!(p.peek(), Some(Tok::PlusPlus) | Some(Tok::MinusMinus)) {
        let inc = matches!(p.peek(), Some(Tok::PlusPlus));
        p.bump();
        let name = match p.bump().cloned() {
            Some(Tok::Ident(s)) => s,
            other => {
                return Err(EmitError::Unsupported(format!(
                    "expected identifier after prefix `++/--` in for-step, got {other:?}"
                )));
            }
        };
        let (target, lvalue) = if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
            (AssignTarget::Local(idx), Expr::Local(idx))
        } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
            (AssignTarget::Global(idx), Expr::Global(idx))
        } else {
            return Err(EmitError::Unsupported(format!(
                "prefix `++/--` of unknown identifier `{name}` in for-clause"
            )));
        };
        return Ok(Stmt::Assign {
            target,
            value: Expr::BinOp {
                op: if inc { BinOp::Add } else { BinOp::Sub },
                left: Box::new(lvalue),
                right: Box::new(Expr::IntLit(1)),
            },
        });
    }
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
    if let Some(value) = parse_compound_rhs(p, &target)? {
        return Ok(Stmt::Assign { target, value });
    }
    p.eat(&Tok::Assign)?;
    let value = parse_expr(p)?;
    Ok(Stmt::Assign { target, value })
}

fn parse_cond(p: &mut Parser<'_>) -> Result<Cond, EmitError> {
    let expr = parse_expr(p)?;
    Ok(cond_from_expr(expr))
}

/// Resolve `<expr>.<field>` or `<expr>-><field>` to its byte offset
/// and field size by looking up `field` in the struct definition at
/// `sidx`. Caller has already consumed `.` or `->`.
fn parse_field_lookup(p: &mut Parser<'_>, sidx: usize) -> Result<(u16, u8), EmitError> {
    let fname = match p.bump().cloned() {
        Some(Tok::Ident(s)) => s,
        other => {
            return Err(EmitError::Unsupported(format!(
                "expected field name, got {other:?}"
            )));
        }
    };
    let sdef = &p.structs[sidx];
    let field = sdef.fields.iter().find(|f| f.name == fname).ok_or_else(|| {
        EmitError::Unsupported(format!(
            "field `{fname}` not in struct `{}`", sdef.name
        ))
    })?;
    Ok((field.byte_off, field.size))
}

/// Convert a parsed expression into a Cond — recognizes `&&` / `||`
/// at the top level so emit_cond_skip can emit short-circuit
/// branches, and unwraps relational ops into `Cond::Cmp`.
fn cond_from_expr(expr: Expr) -> Cond {
    if let Expr::BinOp { op: BinOp::LogAnd, left, right } = expr {
        return Cond::And(
            Box::new(cond_from_expr(*left)),
            Box::new(cond_from_expr(*right)),
        );
    }
    if let Expr::BinOp { op: BinOp::LogOr, left, right } = expr {
        return Cond::Or(
            Box::new(cond_from_expr(*left)),
            Box::new(cond_from_expr(*right)),
        );
    }
    if let Expr::BinOp { op, left, right } = &expr {
        let rel = match op {
            BinOp::Eq => Some(RelOp::Eq),
            BinOp::Ne => Some(RelOp::Ne),
            BinOp::Lt => Some(RelOp::Lt),
            BinOp::Gt => Some(RelOp::Gt),
            BinOp::Le => Some(RelOp::Le),
            BinOp::Ge => Some(RelOp::Ge),
            _ => None,
        };
        if let Some(op) = rel {
            return Cond::Cmp {
                op,
                left: left.as_ref().clone(),
                right: right.as_ref().clone(),
            };
        }
    }
    Cond::Truthy(expr)
}

/// Expression parser — recognizes the Slice-4 shapes:
/// `<atom>` or `<atom> <op> <atom>` where op is `+ - *`.
fn parse_expr(p: &mut Parser<'_>) -> Result<Expr, EmitError> {
    let cond = parse_binop_prec(p, 0)?;
    if matches!(p.peek(), Some(Tok::Quest)) {
        p.bump();
        let then_arm = parse_expr(p)?;
        p.eat(&Tok::Colon)?;
        let else_arm = parse_expr(p)?;
        return Ok(Expr::Ternary {
            cond: Box::new(cond),
            then_arm: Box::new(then_arm),
            else_arm: Box::new(else_arm),
        });
    }
    Ok(cond)
}

/// Operator-precedence climbing for the binary-operator chain. The
/// precedence table matches C's:
///
/// ```text
/// 12  *  /  %
/// 11  +  -
/// 10  << >>
///  9  <  <=  >  >=
///  8  == !=
///  7  &
///  6  ^
///  5  |
///  4  &&
///  3  ||
/// ```
fn parse_binop_prec(p: &mut Parser<'_>, min_prec: u8) -> Result<Expr, EmitError> {
    let mut left = parse_atom(p)?;
    loop {
        let (op, prec) = match p.peek() {
            Some(Tok::Star)    => (BinOp::Mul,    12),
            Some(Tok::Slash)   => (BinOp::Div,    12),
            Some(Tok::Percent) => (BinOp::Mod,    12),
            Some(Tok::Plus)    => (BinOp::Add,    11),
            Some(Tok::Minus)   => (BinOp::Sub,    11),
            Some(Tok::Shl)     => (BinOp::Shl,    10),
            Some(Tok::Shr)     => (BinOp::Shr,    10),
            Some(Tok::Lt)      => (BinOp::Lt,      9),
            Some(Tok::Le)      => (BinOp::Le,      9),
            Some(Tok::Gt)      => (BinOp::Gt,      9),
            Some(Tok::Ge)      => (BinOp::Ge,      9),
            Some(Tok::EqEq)    => (BinOp::Eq,      8),
            Some(Tok::NotEq)   => (BinOp::Ne,      8),
            Some(Tok::Amp)     => (BinOp::BitAnd,  7),
            Some(Tok::Caret)   => (BinOp::BitXor,  6),
            Some(Tok::Pipe)    => (BinOp::BitOr,   5),
            Some(Tok::AndAnd)  => (BinOp::LogAnd,  4),
            Some(Tok::OrOr)    => (BinOp::LogOr,   3),
            _ => break,
        };
        if prec < min_prec { break; }
        p.bump();
        let right = parse_binop_prec(p, prec + 1)?;
        left = Expr::BinOp { op, left: Box::new(left), right: Box::new(right) };
    }
    Ok(left)
}

/// Best-effort pointee-size inference for `*<expr>` lowering.
/// Returns the byte width of `*expr`. `char *` resolves to 1; `int *`
/// (and unrecognized shapes) to 2. Used by parse_atom to pick between
/// `DerefByte` and `DerefWord` variants. Parameters carry no type
/// info in Phase 1 so they default to int-pointer (word).
fn pointee_size_of(e: &Expr, globals: &[Global]) -> usize {
    match e {
        Expr::Global(idx) => globals[*idx].element_size,
        Expr::BinOp { op: BinOp::Add, left, right } => {
            // `<ptr> + K` and `K + <ptr>` both inherit the pointer's
            // pointee size.
            match (left.as_ref(), right.as_ref()) {
                (Expr::Global(idx), _) | (_, Expr::Global(idx)) => {
                    globals[*idx].element_size
                }
                _ => 2,
            }
        }
        _ => 2,
    }
}

fn parse_atom(p: &mut Parser<'_>) -> Result<Expr, EmitError> {
    let tok = p.bump().cloned();
    match tok {
        Some(Tok::LParen) => {
            // `(type) <expr>` cast — recognize a type-keyword right
            // after `(` and treat the cast as identity (Phase 1
            // doesn't model signedness or narrowing semantics).
            skip_decl_modifiers(p);
            if matches!(p.peek(), Some(Tok::Kw("int")) | Some(Tok::Kw("char"))) {
                p.bump();
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                p.eat(&Tok::RParen)?;
                return parse_atom(p);
            }
            let inner = parse_expr(p)?;
            p.eat(&Tok::RParen)?;
            Ok(inner)
        }
        Some(Tok::Int(n)) => Ok(Expr::IntLit(n)),
        Some(Tok::PlusPlus) => {
            // `++<ident>` — synthesize `<ident> + 1` at use site.
            // For statement contexts, parse_stmt routes through
            // parse_compound_rhs instead.
            let inner = parse_atom(p)?;
            Ok(Expr::BinOp {
                op: BinOp::Add,
                left: Box::new(inner),
                right: Box::new(Expr::IntLit(1)),
            })
        }
        Some(Tok::MinusMinus) => {
            let inner = parse_atom(p)?;
            Ok(Expr::BinOp {
                op: BinOp::Sub,
                left: Box::new(inner),
                right: Box::new(Expr::IntLit(1)),
            })
        }
        Some(Tok::Bang) => {
            // `!<expr>` — equivalent to `<expr> == 0`.
            let inner = parse_atom(p)?;
            Ok(Expr::BinOp {
                op: BinOp::Eq,
                left: Box::new(inner),
                right: Box::new(Expr::IntLit(0)),
            })
        }
        Some(Tok::Tilde) => {
            // `~<expr>` — bitwise complement via XOR with all-ones.
            let inner = parse_atom(p)?;
            Ok(Expr::BinOp {
                op: BinOp::BitXor,
                left: Box::new(inner),
                right: Box::new(Expr::IntLit(-1)),
            })
        }
        Some(Tok::StrLit(mut bytes)) => {
            // Intern the literal in the unit-level string pool with
            // the terminating NUL appended. Fixture 4103.
            bytes.push(0);
            let idx = p.strings.len();
            p.strings.push(bytes);
            Ok(Expr::StrLit(idx))
        }
        Some(Tok::Minus) => {
            // Unary minus: `- <atom>`. For literals, fold immediately;
            // otherwise lower to `0 - <atom>` for the existing
            // arithmetic codegen to handle.
            let inner = parse_atom(p)?;
            if let Expr::IntLit(n) = inner {
                Ok(Expr::IntLit(n.wrapping_neg()))
            } else {
                Ok(Expr::BinOp {
                    op: BinOp::Sub,
                    left: Box::new(Expr::IntLit(0)),
                    right: Box::new(inner),
                })
            }
        }
        Some(Tok::Kw("sizeof")) => {
            // `sizeof(<type>)` or `sizeof <expr>` — evaluated at
            // parse time into an int literal. We support int/char,
            // pointer-to-X (always 2 in small model), `struct Name`,
            // and a bare identifier (local / global storage size).
            let has_paren = matches!(p.peek(), Some(Tok::LParen));
            if has_paren { p.bump(); }
            let n = if let Some(Tok::Kw("struct")) = p.peek().cloned() {
                p.bump();
                let sname = match p.bump().cloned() {
                    Some(Tok::Ident(s)) => s,
                    other => {
                        return Err(EmitError::Unsupported(format!(
                            "expected struct name in sizeof, got {other:?}"
                        )));
                    }
                };
                p.structs.iter().find(|s| s.name == sname)
                    .map(|s| s.total_bytes as i32)
                    .ok_or_else(|| EmitError::Unsupported(format!("unknown struct `{sname}` in sizeof")))?
            } else if let Some(Tok::Kw("int")) = p.peek().cloned() {
                p.bump();
                while matches!(p.peek(), Some(Tok::Star)) { p.bump(); }
                2 // int or any int-pointer = 2 in small model
            } else if let Some(Tok::Kw("char")) = p.peek().cloned() {
                p.bump();
                if matches!(p.peek(), Some(Tok::Star)) {
                    p.bump();
                    2
                } else {
                    1
                }
            } else if let Some(Tok::Ident(name)) = p.peek().cloned() {
                p.bump();
                if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                    p.local_specs[idx].storage_bytes() as i32
                } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                    let g = &p.globals[idx];
                    (g.element_size * g.array_len) as i32
                } else {
                    return Err(EmitError::Unsupported(format!(
                        "sizeof unknown identifier `{name}`"
                    )));
                }
            } else {
                return Err(EmitError::Unsupported(format!(
                    "unsupported sizeof operand: {:?}", p.peek()
                )));
            };
            if has_paren { p.eat(&Tok::RParen)?; }
            Ok(Expr::IntLit(n))
        }
        Some(Tok::Amp) => {
            // Address-of `&<ident>`. Phase 1 supports globals and
            // locals; locals lower to `lea ax, [bp-disp]`.
            let name = match p.bump().cloned() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(EmitError::Unsupported(format!(
                        "expected identifier after `&`, got {other:?}"
                    )));
                }
            };
            if let Some(idx) = p.local_names.iter().position(|n| *n == name) {
                return Ok(Expr::AddrOfLocal(idx));
            }
            let idx = p.global_names.iter().position(|n| *n == name)
                .ok_or_else(|| EmitError::Unsupported(format!(
                    "address-of unknown identifier `{name}`"
                )))?;
            Ok(Expr::AddrOfGlobal(idx))
        }
        Some(Tok::Star) => {
            // Unary deref `*<expr>`. Pick the byte- vs word-sized
            // variant from the inner expression's pointee type.
            let inner = parse_atom(p)?;
            let pointee_size = pointee_size_of(&inner, &p.globals);
            if pointee_size == 1 {
                Ok(Expr::DerefByte { ptr: Box::new(inner) })
            } else {
                Ok(Expr::DerefWord { ptr: Box::new(inner) })
            }
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
                // `<local>[<expr>]` — element access on a local
                // array. Picks the byte-load + cbw variant for char
                // arrays, word load otherwise.
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    return if p.local_specs[idx].size == 1 {
                        Ok(Expr::LocalIndexByte { local: idx, index: Box::new(index) })
                    } else {
                        Ok(Expr::LocalIndex { local: idx, index: Box::new(index) })
                    };
                }
                // `<struct-local>.<field>` member access.
                if matches!(p.peek(), Some(Tok::Dot))
                    && let Some(sidx) = p.local_specs[idx].struct_idx
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    return Ok(Expr::LocalField { local: idx, byte_off, size });
                }
                // `<struct-ptr-local>-><field>` member access.
                if matches!(p.peek(), Some(Tok::Arrow))
                    && let Some(sidx) = p.local_specs[idx].struct_idx
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    return Ok(Expr::DerefLocalField { ptr_local: idx, byte_off, size });
                }
                Ok(Expr::Local(idx))
            } else if let Some(idx) = p.param_names.iter().position(|n| *n == name) {
                // `<param>[K]` for `int *p` / `int p[]` parameters →
                // load ptr into BX, then word load `mov ax, [bx+K*2]`
                // (`8b 47 disp`). Phase 1 keeps the disp in disp8.
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    return Ok(Expr::ParamIndex { param: idx, index: Box::new(index) });
                }
                // `<struct-ptr-param>-><field>` member access.
                if matches!(p.peek(), Some(Tok::Arrow))
                    && let Some(Some(sidx)) = p.param_struct_idxs.get(idx).cloned()
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    return Ok(Expr::DerefParamField { ptr_param: idx, byte_off, size });
                }
                Ok(Expr::Param(idx))
            } else if let Some(idx) = p.global_names.iter().position(|n| *n == name) {
                // `<global>[<expr>]` — array index or pointer index.
                // Array (`int a[N]`): direct addressing.
                // Pointer (`char *p`): load pointer first, then offset.
                if matches!(p.peek(), Some(Tok::LBrack)) {
                    p.bump();
                    let index = parse_expr(p)?;
                    p.eat(&Tok::RBrack)?;
                    let g = &p.globals[idx];
                    if g.is_pointer {
                        // Pointer-indexed read. Phase 1 covers the
                        // `char *p` byte form (fixture 4123).
                        Ok(Expr::PtrIndexByte { ptr: idx, index: Box::new(index) })
                    } else if g.element_size == 1 {
                        Ok(Expr::IndexByte { array: idx, index: Box::new(index) })
                    } else {
                        Ok(Expr::Index { array: idx, index: Box::new(index) })
                    }
                } else if matches!(p.peek(), Some(Tok::Dot))
                    && let Some(sidx) = p.globals[idx].struct_idx
                {
                    p.bump();
                    let (byte_off, size) = parse_field_lookup(p, sidx)?;
                    Ok(Expr::GlobalField { global: idx, byte_off, size })
                } else {
                    // Array name in non-subscript position decays to
                    // a pointer (the array's base address). Scalar
                    // globals stay as values.
                    let g = &p.globals[idx];
                    if !g.is_pointer && g.array_len > 1 && g.struct_idx.is_none() && !g.is_long {
                        Ok(Expr::AddrOfGlobal(idx))
                    } else {
                        Ok(Expr::Global(idx))
                    }
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
    let long_globals: Vec<bool> = unit.globals.iter().map(|g| g.is_long).collect();
    let function_emits: Vec<FunctionEmit> = unit
        .functions
        .iter()
        .map(|f| emit_function(f, &long_globals))
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

    // String-pool offsets in CONST. MSC aligns each string to a
    // 2-byte boundary, leaving a zero pad byte after any string of
    // odd length (fixture 4113). The last entry isn't padded — the
    // CONST segment ends at the end of its trailing string.
    let mut string_offsets: Vec<usize> = Vec::with_capacity(unit.strings.len());
    let mut const_cursor: usize = 0;
    for (i, s) in unit.strings.iter().enumerate() {
        string_offsets.push(const_cursor);
        const_cursor += s.len();
        if i + 1 < unit.strings.len() && const_cursor % 2 != 0 {
            const_cursor += 1;
        }
    }
    let const_len = u16::try_from(const_cursor).expect("CONST length fits in u16");

    // _DATA layout — every initialized global gets 2 bytes (int) in
    // source order. Uninitialized globals (tentative definitions)
    // don't contribute here; they'll go through COMDEF in a later
    // sub-slice.
    let mut data_offsets: Vec<Option<usize>> = Vec::with_capacity(unit.globals.len());
    let mut data_cursor: usize = 0;
    for g in &unit.globals {
        if let Some(values) = &g.init {
            data_offsets.push(Some(data_cursor));
            let bytes: usize = values.iter().map(|v| v.size_bytes()).sum();
            data_cursor += bytes;
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
                let byte_len = g.storage_bytes();
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

    // PUBDEFs — MSC walks definitions in source order and starts a
    // new PUBDEF record on each (group, segment) transition. So
    // `_get; int g; _main;` becomes three records (text → data →
    // text), while consecutive same-bucket symbols share a record.
    // Fixtures 4104 (data first), 4099 (text only), 4125 (interleaved).
    //
    // Buckets:
    //   _TEXT: (group 0, seg 1) — functions
    //   _DATA: (group 1 = DGROUP, seg 2) — initialized globals
    let mut current: Option<(u8, u8, Vec<u8>)> = None;
    let flush = |b: &mut ObjBuilder, cur: &mut Option<(u8, u8, Vec<u8>)>| {
        if let Some((grp, seg, payload)) = cur.take() {
            let mut rec = Vec::with_capacity(payload.len() + 2);
            rec.push(grp);
            rec.push(seg);
            rec.extend_from_slice(&payload);
            b.write_record(obj::PUBDEF_16, &rec);
        }
    };
    for entry in &unit.decl_order {
        let (grp, seg, sym, off) = match entry {
            TopDecl::Global(i) => {
                let Some(off) = data_offsets[*i] else { continue };
                let sym = symbol_name(&unit.globals[*i].name);
                let off = u16::try_from(off).expect("offset fits");
                (1u8, 2u8, sym, off)
            }
            TopDecl::Function(i) => {
                let sym = symbol_name(&unit.functions[*i].name);
                let off = u16::try_from(function_offsets[*i]).expect("offset fits");
                (0u8, 1u8, sym, off)
            }
        };
        let same_bucket = matches!(&current, Some((g, s, _)) if *g == grp && *s == seg);
        if !same_bucket {
            flush(&mut b, &mut current);
            current = Some((grp, seg, Vec::new()));
        }
        let payload = &mut current.as_mut().unwrap().2;
        payload.push(u8::try_from(sym.len()).expect("pubdef name fits"));
        payload.extend_from_slice(sym.as_bytes());
        payload.extend_from_slice(&off.to_le_bytes());
        payload.push(0); // type idx
    }
    flush(&mut b, &mut current);

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
                    // Patch the placeholder bytes with the string's
                    // CONST offset so the linker (which adds the
                    // CONST base) lands at the right byte. Fixture
                    // 4128 has multiple strings — without this patch
                    // every StrLoad would resolve to the first
                    // string.
                    let off = u16::try_from(string_offsets[*string_idx])
                        .expect("string offset fits");
                    fe.bytes[fx.body_offset + 1] = (off & 0xFF) as u8;
                    fe.bytes[fx.body_offset + 2] = ((off >> 8) & 0xFF) as u8;
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset + 1,
                        kind: FixupKind::StrLoad { string_idx: *string_idx },
                    });
                }
                FixupKind::GlobalAddr { global_idx } => {
                    // Patch placeholder bytes with the global's
                    // in-_DATA offset for PUBDEF targets. COMDEF
                    // targets keep their zero placeholder — the
                    // linker substitutes via the EXTDEF FIXUP and
                    // ignores the displacement. Fixture 4138 has
                    // `b` at _DATA offset 2; without this patch
                    // every PUBDEF-global access resolves to the
                    // first global.
                    if let Some(off) = data_offsets[*global_idx] {
                        let off = u16::try_from(off).expect("global offset fits");
                        let existing = u16::from_le_bytes([
                            fe.bytes[fx.body_offset + 1],
                            fe.bytes[fx.body_offset + 2],
                        ]);
                        // Combine with whatever the codegen wrote
                        // (e.g. constant array index 4109 wrote
                        // `2*K`). Patch ADD, not replace.
                        let patched = existing.wrapping_add(off);
                        fe.bytes[fx.body_offset + 1] = (patched & 0xFF) as u8;
                        fe.bytes[fx.body_offset + 2] = ((patched >> 8) & 0xFF) as u8;
                    }
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset + 1,
                        kind: FixupKind::GlobalAddr { global_idx: *global_idx },
                    });
                }
            }
        }
    }

    // LEDATA — CONST segment. MSC packs consecutive strings into
    // one LEDATA when no padding is needed. When an odd-length
    // string forces a 1-byte pad before the next string, MSC closes
    // the current LEDATA and opens a new one at the aligned offset.
    // Fixtures: 4110 (1 string), 4128 (2 even-length → 1 LEDATA),
    // 4113 (2 odd-length → 2 LEDATAs with a gap), 4132 (mixed).
    if !unit.strings.is_empty() {
        let mut current_start = string_offsets[0];
        let mut current_bytes: Vec<u8> = Vec::new();
        for (i, s) in unit.strings.iter().enumerate() {
            current_bytes.extend_from_slice(s);
            let next_aligned = i + 1 < unit.strings.len()
                && (string_offsets[i] + s.len()) != string_offsets[i + 1];
            if next_aligned {
                let off = u16::try_from(current_start).expect("CONST offset fits");
                b.write_ledata16(3, off, &current_bytes);
                current_bytes.clear();
                current_start = string_offsets[i + 1];
            }
        }
        if !current_bytes.is_empty() {
            let off = u16::try_from(current_start).expect("CONST offset fits");
            b.write_ledata16(3, off, &current_bytes);
        }
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
                        GlobalInit::Byte(b) => {
                            data_bytes.push(*b);
                        }
                        GlobalInit::StrAddr(si) => {
                            // Placeholder = the string's CONST offset.
                            // FIXUP uses P=1 (no displacement), so the
                            // linker reads this slot and adds the
                            // CONST segment base.
                            let off = u16::try_from(string_offsets[*si])
                                .expect("string offset fits");
                            data_bytes.extend_from_slice(&off.to_le_bytes());
                        }
                        GlobalInit::GlobalAddr(gi) => {
                            // Placeholder = _DATA offset of the target if
                            // it's initialized (PUBDEF path), else 0 — for
                            // a COMDEF target the linker substitutes the
                            // address via the EXTDEF FIXUP.
                            let off = match data_offsets[*gi] {
                                Some(o) => u16::try_from(o).expect("global offset fits"),
                                None => 0,
                            };
                            data_bytes.extend_from_slice(&off.to_le_bytes());
                        }
                    }
                }
            }
        }
        b.write_ledata16(2, 0, &data_bytes);
        // Collect per FIXUP'd slot. Variants:
        //   StrAddr      → `c4 off 9c`    (DGROUP frame + CONST thread)
        //   GlobalAddr → PUBDEF target → `c4 off 9d` (DGROUP + _DATA thread)
        //                COMDEF target → `c4 off 56 idx` (target's frame
        //                via EXTDEF, fixture 4116)
        enum DataFx { Thread(u8), Ext(u8) }
        let mut data_slot_fixups: Vec<(u8, DataFx)> = Vec::new();
        let mut off: usize = 0;
        for g in &unit.globals {
            if let Some(values) = &g.init {
                for v in values {
                    let slot_off = u8::try_from(off).expect("data fixup offset fits");
                    match v {
                        GlobalInit::StrAddr(_) => {
                            data_slot_fixups.push((slot_off, DataFx::Thread(0x9C)));
                        }
                        GlobalInit::GlobalAddr(gi) => {
                            if unit.globals[*gi].init.is_some() {
                                data_slot_fixups.push((slot_off, DataFx::Thread(0x9D)));
                            } else {
                                let sym = symbol_name(&unit.globals[*gi].name);
                                let idx = *extdef_idx_of.get(&sym).unwrap_or_else(|| {
                                    panic!("EXTDEF index missing for COMDEF `{sym}`")
                                });
                                data_slot_fixups.push((slot_off, DataFx::Ext(idx)));
                            }
                        }
                        GlobalInit::Int(_) | GlobalInit::Byte(_) => {}
                    }
                    off += v.size_bytes();
                }
            }
        }
        // MSC sorts FIXUPs within a record by descending offset.
        data_slot_fixups.sort_by(|a, b| b.0.cmp(&a.0));
        let mut data_fixups: Vec<u8> = Vec::new();
        for (off, fx) in &data_slot_fixups {
            match fx {
                DataFx::Thread(byte) => {
                    data_fixups.extend_from_slice(&[0xC4, *off, *byte]);
                }
                DataFx::Ext(idx) => {
                    data_fixups.extend_from_slice(&[0xC4, *off, 0x56, *idx]);
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
/// Straight-line const-prop over locals + globals. Each assign of
/// a literal to a Local/Global makes subsequent reads of that name
/// fold to the literal; assigns of non-literal expressions remove
/// the entry. Control-flow nodes (`if`, loops, blocks containing
/// them) clear both tables conservatively. Fixture 4106 motivates
/// the global side; fixture 1020 needs the local side.
#[derive(Default)]
struct ConstProp {
    g_known: std::collections::HashMap<usize, i32>,
    l_known: std::collections::HashMap<usize, i32>,
    /// Local array element values keyed by (local_idx, byte_off).
    /// Matches the IndexedLocal/IndexedLocalByte byte_off so a
    /// `<local>[K] = V; return <local>[K];` round-trip folds.
    la_known: std::collections::HashMap<(usize, u16), i32>,
    /// Globals that hold a 4-byte long. Skipped by the
    /// `Global(g) → IntLit(K)` substitution pass so compound updates
    /// like `g += K` and `g = g + K` keep `Global(g)` on the left for
    /// the long-specific assign codegen to recognize.
    long_globals: std::collections::HashSet<usize>,
}

fn const_prop_globals(stmts: &[Stmt], local_specs: &[LocalSpec], long_globals: &[bool]) -> Vec<Stmt> {
    let mut cp = ConstProp::default();
    for (i, &is_long) in long_globals.iter().enumerate() {
        if is_long { cp.long_globals.insert(i); }
    }
    // Pre-seed l_known with the locals' constant inits so the
    // const-fold pass sees `int x = 1; switch(x)` as having x=1
    // without re-deriving it from prologue stores.
    for (i, spec) in local_specs.iter().enumerate() {
        // Skip long locals here: substituting `Local(c)` → `IntLit(K)`
        // would make `return (int)c;` emit a const load instead of a
        // slot read (fixture 1037). The emit-time fold_cond path still
        // sees the init via `locals.inits` for cond elision (1632).
        if spec.is_long { continue; }
        // Char locals: only literal-init chars are substituted. A
        // char initialized via local-var arithmetic (`char c = a+b;`)
        // stays as `Local(c)` so bare reads go through slot+cbw
        // (fixture 1046). Literal init (`char c = 'A'+1;`) folds (1023).
        if spec.size == 1 && !spec.init_is_literal { continue; }
        if let Some(k) = spec.init {
            cp.l_known.insert(i, k);
        }
    }
    stmts.iter().map(|s| {
        let mut new_stmt = s.clone();
        prop_stmt(&mut new_stmt, &mut cp);
        new_stmt
    }).collect()
}

fn prop_stmt(stmt: &mut Stmt, cp: &mut ConstProp) {
    match stmt {
        Stmt::Return(e) => prop_expr(e, cp),
        Stmt::ExprStmt(e) => prop_expr(e, cp),
        Stmt::Assign { target, value } => {
            // `x = x op RHS` preserves the `Local(x)` on the left so
            // emit_assign can hit the in-place inc/dec/add/sub-mem
            // peepholes (fixtures 1029, 1116). Substituting `x` to its
            // known IntLit defeats those.
            // For `x = x ± RHS`, leave the LHS unsubstituted so
            // emit_assign sees `BinOp(Local(x), Add|Sub, ...)` and
            // hits the in-place inc/dec/add/sub peephole. Other ops
            // (Shl, Mul, etc.) don't have a peephole shape, so we
            // substitute normally (lets the value const-fold via
            // both operands; fixtures 1022, 1024).
            let self_assign_addsub = matches!(
                (target.clone(), value.clone()),
                (AssignTarget::Local(t), Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, .. })
                    if matches!(left.as_ref(), Expr::Local(l) if *l == t)
            );
            if self_assign_addsub
                && let Expr::BinOp { right, .. } = value
            {
                prop_expr(right, cp);
            } else {
                prop_expr(value, cp);
            }
            match target {
                AssignTarget::Global(g) => {
                    if let Expr::IntLit(k) = value {
                        cp.g_known.insert(*g, *k);
                    } else {
                        cp.g_known.remove(g);
                    }
                }
                AssignTarget::Local(l) => {
                    if let Expr::IntLit(k) = value {
                        cp.l_known.insert(*l, *k);
                    } else {
                        cp.l_known.remove(l);
                    }
                }
                AssignTarget::IndexedLocal { local, byte_off }
                | AssignTarget::IndexedLocalByte { local, byte_off }
                | AssignTarget::LocalField { local, byte_off, .. } => {
                    // Try to fold the value once more — after
                    // prop_expr's leaf substitution the BinOp may
                    // have two literal operands ready to collapse.
                    if let Some(k) = value.fold(&[]) {
                        *value = Expr::IntLit(k);
                    }
                    if let Expr::IntLit(k) = value {
                        cp.la_known.insert((*local, *byte_off), *k);
                    } else {
                        cp.la_known.remove(&(*local, *byte_off));
                    }
                }
                _ => {}
            }
        }
        Stmt::Empty => {}
        Stmt::If { cond, then_branch, else_branch } => {
            // Fold the cond using current knowledge, then propagate
            // into each branch with an isolated copy so writes don't
            // leak across paths. After the if, conservatively clear.
            prop_cond(cond, cp);
            let mut sub = cp_clone(cp);
            prop_stmt(then_branch, &mut sub);
            if let Some(eb) = else_branch {
                let mut sub2 = cp_clone(cp);
                prop_stmt(eb, &mut sub2);
            }
            // After a branch we don't know which path was taken.
            cp.g_known.clear();
            cp.l_known.clear();
        }
        Stmt::Block(stmts) => {
            for s in stmts {
                prop_stmt(s, cp);
            }
        }
        Stmt::Switch { scrutinee, cases } => {
            prop_expr(scrutinee, cp);
            // If the scrutinee folds, replace the switch with the
            // matched case's body (up to the first Stmt::Break).
            if let Expr::IntLit(k) = scrutinee {
                let mut chosen: Option<usize> = None;
                let mut default: Option<usize> = None;
                for (i, arm) in cases.iter().enumerate() {
                    match arm.value {
                        Some(v) if v == *k => { chosen = Some(i); break; }
                        Some(_) => {}
                        None => { default = Some(i); }
                    }
                }
                let pick = chosen.or(default);
                let body: Vec<Stmt> = match pick {
                    Some(i) => {
                        // Take statements up to the first Break.
                        let mut out = Vec::new();
                        let mut j = i;
                        'outer: while j < cases.len() {
                            for s in &cases[j].body {
                                if matches!(s, Stmt::Break) { break 'outer; }
                                out.push(s.clone());
                            }
                            j += 1;
                        }
                        out
                    }
                    None => Vec::new(),
                };
                *stmt = Stmt::Block(body);
                // Recurse into the rewritten Block.
                if let Stmt::Block(stmts) = stmt {
                    for s in stmts { prop_stmt(s, cp); }
                }
            } else {
                // Runtime scrutinee — leave as Switch; codegen
                // will panic until we add real lowering.
                cp.g_known.clear();
                cp.l_known.clear();
                cp.la_known.clear();
            }
        }
        Stmt::Break | Stmt::Continue => {
            // These are flow-control markers; the const-folded
            // switch walker handles Break. Outside that path they
            // signal that subsequent statements may be unreachable.
        }
        _ => {
            // While / for / do-while: fold any cond / step we can
            // reach via a shallow walk, then drop everything.
            cp.g_known.clear();
            cp.l_known.clear();
            cp.la_known.clear();
        }
    }
}

fn cp_clone(cp: &ConstProp) -> ConstProp {
    ConstProp {
        g_known: cp.g_known.clone(),
        l_known: cp.l_known.clone(),
        la_known: cp.la_known.clone(),
        long_globals: cp.long_globals.clone(),
    }
}

fn prop_cond(cond: &mut Cond, cp: &ConstProp) {
    match cond {
        Cond::Truthy(e) => prop_expr(e, cp),
        Cond::Cmp { left, right, .. } => {
            prop_expr(left, cp);
            prop_expr(right, cp);
        }
        Cond::And(a, b) | Cond::Or(a, b) => {
            prop_cond(a, cp);
            prop_cond(b, cp);
        }
    }
}

fn prop_expr(e: &mut Expr, cp: &ConstProp) {
    match e {
        Expr::Global(idx) => {
            // Long globals are never substituted — their compound
            // updates need `Global(g)` on the lhs for the long-specific
            // assign-codegen path to fire (fixture 207).
            if !cp.long_globals.contains(idx)
                && let Some(&k) = cp.g_known.get(idx)
            {
                *e = Expr::IntLit(k);
            }
        }
        Expr::Local(idx) => {
            if let Some(&k) = cp.l_known.get(idx) {
                *e = Expr::IntLit(k);
            }
        }
        Expr::BinOp { left, right, .. } => {
            prop_expr(left, cp);
            prop_expr(right, cp);
        }
        Expr::Call { args, .. } => {
            for a in args {
                prop_expr(a, cp);
            }
        }
        Expr::Index { index, .. }
        | Expr::IndexByte { index, .. }
        | Expr::PtrIndexByte { index, .. } => {
            prop_expr(index, cp);
        }
        Expr::ParamIndex { index, .. } => {
            prop_expr(index, cp);
        }
        Expr::LocalField { .. } => {
            // Substitute the field's known value via la_known
            // keyed by (local, byte_off).
            if let Expr::LocalField { local, byte_off, .. } = e
                && let Some(&v) = cp.la_known.get(&(*local, *byte_off))
            {
                *e = Expr::IntLit(v);
            }
        }
        Expr::GlobalField { .. } => {
            // No const-prop tracking for global struct fields yet.
        }
        Expr::DerefLocalField { .. } | Expr::DerefParamField { .. } => {
            // Pointer-aliasing const-prop not yet implemented.
        }
        Expr::LocalIndex { .. } | Expr::LocalIndexByte { .. } => {
            // Borrow index and substitute *e with the known element
            // value when the index folds and we've tracked it.
            let (local, elem_size, known_k) = match e {
                Expr::LocalIndex { local, index } => {
                    prop_expr(index, cp);
                    let k = if let Expr::IntLit(k) = index.as_ref() { Some(*k) } else { None };
                    (*local, 2u16, k)
                }
                Expr::LocalIndexByte { local, index } => {
                    prop_expr(index, cp);
                    let k = if let Expr::IntLit(k) = index.as_ref() { Some(*k) } else { None };
                    (*local, 1u16, k)
                }
                _ => unreachable!(),
            };
            if let Some(k) = known_k
                && let Ok(byte_off) = u16::try_from(k as i64 * elem_size as i64)
                && let Some(&v) = cp.la_known.get(&(local, byte_off))
            {
                *e = Expr::IntLit(v);
            }
        }
        Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => {
            prop_expr(ptr, cp);
        }
        Expr::AddrOfGlobal(_) | Expr::AddrOfLocal(_) => {}
        Expr::Ternary { cond, then_arm, else_arm } => {
            prop_expr(cond, cp);
            prop_expr(then_arm, cp);
            prop_expr(else_arm, cp);
        }
        Expr::IntLit(_) | Expr::Param(_) | Expr::StrLit(_) => {}
    }
}

fn emit_function(func: &Function, long_globals: &[bool]) -> FunctionEmit {
    let body = const_prop_globals(&func.body, &func.locals, long_globals);
    // Extract a `Vec<Option<i32>>` view for the existing fold path —
    // saves rewriting every codegen helper to know about LocalSpec.
    let local_inits: Vec<Option<i32>> = func.locals.iter().map(|l| l.init).collect();
    let local_long: Vec<bool> = func.locals.iter().map(|l| l.is_long).collect();
    let local_literals: Vec<bool> = func.locals.iter().map(|l| l.init_is_literal).collect();
    let mut bytes = Vec::with_capacity(32);
    let mut fixups: Vec<Fixup> = Vec::new();
    let frame = Frame::for_function(func);
    // Each local — int or char — occupies one 2-byte slot in MSC's
    // frame (confirmed by fixtures 4080, 1010, 1305). Char slots use
    // byte instructions for load/store but still consume a word.
    //
    // Layout: source order. First-declared local gets the shallowest
    // slots (closest to BP); arrays occupy `array_len` contiguous
    // word/byte slots with element 0 at the deepest disp.
    let mut local_disps: Vec<i16> = Vec::with_capacity(func.locals.len());
    let mut cumulative: i32 = 0;
    for spec in &func.locals {
        let own = i32::try_from(spec.storage_bytes()).expect("local fits");
        cumulative += own;
        local_disps.push(-i16::try_from(cumulative).expect("local disp fits"));
    }
    let local_sizes: Vec<usize> = func.locals.iter().map(|l| l.size).collect();
    let frame_bytes: usize = cumulative as usize;

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

    // Initialized-local writes — `int x = K;` → `c7 46 disp lo hi`;
    // `char x = K;` → `c6 46 disp imm8`. Arrays don't get a constant
    // init here; their element stores live in the prelude body the
    // parser synthesized.
    for (i, spec) in func.locals.iter().enumerate() {
        if let Some(value) = spec.init {
            let disp = local_disps[i];
            if spec.is_long {
                let low = (value as u32 & 0xFFFF) as u16;
                let high = (((value as i32) >> 16) as u32 & 0xFFFF) as u16;
                if low == 0 && high == 0 {
                    // Zero-init peephole: `xor ax, ax; mov [bp-d_hi],
                    // ax; mov [bp-d_lo], ax` — 8 bytes vs 10 for the
                    // two `c7 46` stores. Fixture 1737.
                    bytes.extend_from_slice(&[0x2B, 0xC0]);
                    bytes.push(0x89);
                    bytes.push(0x46);
                    bytes.push((disp + 2) as u8);
                    bytes.push(0x89);
                    bytes.push(0x46);
                    bytes.push(disp as u8);
                } else {
                    // Long: two word stores. `c7 46 disp_low <lo>`
                    // then `c7 46 disp_high <hi>`.
                    bytes.push(0xC7);
                    bytes.push(0x46);
                    bytes.push(disp as u8);
                    bytes.extend_from_slice(&low.to_le_bytes());
                    bytes.push(0xC7);
                    bytes.push(0x46);
                    bytes.push((disp + 2) as u8);
                    bytes.extend_from_slice(&high.to_le_bytes());
                }
            } else if spec.size == 1 {
                let imm = (value as u32 & 0xFF) as u8;
                bytes.push(0xC6);
                bytes.push(0x46);
                bytes.push(disp as u8);
                bytes.push(imm);
            } else {
                let imm = (value as u32 & 0xFFFF) as u16;
                bytes.push(0xC7);
                bytes.push(0x46);
                bytes.push(disp as u8);
                bytes.extend_from_slice(&imm.to_le_bytes());
            }
        }
    }

    let locals_view = Locals {
        inits: &local_inits,
        disps: &local_disps,
        sizes: &local_sizes,
        long_globals,
        long_locals: &local_long,
        init_literals: &local_literals,
    };

    let mut reachable = true;
    for stmt in &body {
        if !reachable {
            break;
        }
        emit_stmt(
            stmt,
            &locals_view,
            frame,
            func.return_int,
            &mut bytes,
            &mut fixups,
        );
        if stmt_always_returns(stmt, &locals_view) {
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
    locals: &Locals<'_>,
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
        Stmt::Switch { .. } => {
            panic!("non-foldable switch not yet supported (const-prop should have rewritten)")
        }
        Stmt::Break => {
            panic!("break outside const-folded switch not yet supported")
        }
        Stmt::Continue => {
            panic!("continue not yet supported")
        }
        Stmt::Block(stmts) => {
            // Block has no scoping at the codegen level. Sub-statements
            // emit inline. Const-prop's already been applied at the
            // function level before we got here.
            let mut reachable = true;
            for s in stmts {
                if !reachable { break; }
                emit_stmt(s, locals, frame, return_int, out, fixups);
                if stmt_always_returns(s, locals) {
                    reachable = false;
                }
            }
        }
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
            emit_cond_skip(cond, take_then_disp, locals, out, fixups);
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
fn stmt_always_returns(stmt: &Stmt, locals: &Locals<'_>) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::Empty => false,
        Stmt::ExprStmt(_) | Stmt::Assign { .. } => false,
        // Loops with a runtime cond can fall through; the
        // const-true infinite-loop case isn't exercised yet so we
        // conservatively answer false.
        Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } => false,
        Stmt::Switch { .. } | Stmt::Break | Stmt::Continue => false,
        Stmt::Block(stmts) => stmts.iter().any(|s| stmt_always_returns(s, locals)),
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
fn fold_cond(cond: &Cond, locals: &Locals<'_>) -> Option<i32> {
    match cond {
        Cond::Truthy(e) => e.fold(locals.inits),
        Cond::Cmp { op, left, right } => {
            let l = left.fold(locals.inits)?;
            let r = right.fold(locals.inits)?;
            Some(match op {
                RelOp::Eq => i32::from(l == r),
                RelOp::Ne => i32::from(l != r),
                RelOp::Lt => i32::from(l < r),
                RelOp::Gt => i32::from(l > r),
                RelOp::Le => i32::from(l <= r),
                RelOp::Ge => i32::from(l >= r),
            })
        }
        Cond::And(a, b) => {
            let av = fold_cond(a, locals)?;
            if av == 0 { return Some(0); }
            let bv = fold_cond(b, locals)?;
            Some(i32::from(bv != 0))
        }
        Cond::Or(a, b) => {
            let av = fold_cond(a, locals)?;
            if av != 0 { return Some(1); }
            let bv = fold_cond(b, locals)?;
            Some(i32::from(bv != 0))
        }
    }
}

fn emit_return(
    expr: &Expr,
    locals: &Locals<'_>,
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
        } else if matches!(expr, Expr::Local(i) if locals.is_long_local(*i)) {
            // Long locals are read through the 4-byte slot — MSC
            // emits `mov ax, [bp-disp_low]` even when the value is
            // known at compile time (fixture 1037).
            emit_expr_to_ax(expr, locals, out, fixups);
        } else if matches!(expr, Expr::Local(_)) {
            // Const-prop replaces known `Local(i)` with `IntLit(K)`
            // upstream — so anything still showing as `Local` at this
            // point either wasn't tracked (long, computed-char) or has
            // been mutated past its declaration value. Always load,
            // never fold from the static `spec.init` view (would
            // misread mutated scalars; fixture 1029).
            emit_expr_to_ax(expr, locals, out, fixups);
        } else if let Some(k) = expr.fold(locals.inits) {
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
    locals: &Locals<'_>,
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
fn emit_push_arg(arg: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match arg {
        Expr::IntLit(k) => {
            let imm = (*k as u32 & 0xFFFF) as u16;
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
            out.push(0x50); // push ax
        }
        Expr::Local(idx) => {
            // `push word ptr [bp-disp]` — `FF /6 r/m`.
            let disp = locals.disp(*idx);
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
        Expr::AddrOfGlobal(idx) => {
            // `mov ax, 00 00` placeholder; FIXUP carries the global's
            // address back into the imm16 at link time. Fixture 4125.
            let body_offset = out.len();
            out.extend_from_slice(&[0xB8, 0x00, 0x00]);
            out.push(0x50); // push ax
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *idx },
            });
        }
        Expr::Global(idx) => {
            // `ff 36 <addr>` — push word ptr [imm16]. The placeholder
            // gets the global's _DATA offset; the FIXUP carries the
            // base. Fixture 4131.
            out.push(0xFF);
            let body_offset = out.len();
            out.extend_from_slice(&[0x36, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *idx },
            });
        }
        Expr::BinOp { .. } => {
            // Computed value: build the result in AX then push.
            // Fixture 4144.
            emit_expr_to_ax(arg, locals, out, fixups);
            out.push(0x50);
        }
        other => panic!("argument shape not yet supported: {other:?}"),
    }
}

/// `<local> = <expr>;`. Phase 1 supports the peephole
/// `<local> = <same-local> ± 1;` → `inc/dec word ptr [bp-disp]`
/// (fixture 4096: `x = x - 1;` in a while body). The general path
/// — `mov ax, <expr>; mov [bp-disp], ax` — is reserved for a
/// future fixture that exercises a non-peephole shape.
fn emit_assign(target: AssignTarget, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let local_idx = match target {
        AssignTarget::Local(i) => i,
        AssignTarget::Global(g) => {
            return emit_assign_global(g, value, locals, out, fixups);
        }
        AssignTarget::DerefGlobal(g) => {
            return emit_assign_deref_global(g, value, locals, out, fixups);
        }
        AssignTarget::DerefLocal(li) => {
            return emit_assign_deref_local(li, value, locals, out, fixups);
        }
        AssignTarget::IndexedGlobal { array, byte_off } => {
            return emit_assign_indexed_global(array, byte_off, value, locals, out, fixups);
        }
        AssignTarget::IndexedGlobalByte { array, byte_off } => {
            return emit_assign_indexed_global_byte(array, byte_off, value, locals, out, fixups);
        }
        AssignTarget::PtrIndexByte { ptr, disp } => {
            return emit_assign_ptr_index_byte(ptr, disp, value, locals, out, fixups);
        }
        AssignTarget::IndexedLocal { local, byte_off } => {
            return emit_assign_indexed_local(local, byte_off, value, locals, out, fixups);
        }
        AssignTarget::IndexedLocalByte { local, byte_off } => {
            return emit_assign_indexed_local_byte(local, byte_off, value, locals, out, fixups);
        }
        AssignTarget::LocalField { local, byte_off, size } => {
            return emit_assign_local_field(local, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::DerefLocalField { ptr_local, byte_off, size } => {
            return emit_assign_deref_local_field(ptr_local, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::GlobalField { global, byte_off, size } => {
            return emit_assign_global_field(global, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::DerefParamField { ptr_param, byte_off, size } => {
            return emit_assign_deref_param_field(ptr_param, byte_off, size, value, locals, out, fixups);
        }
    };
    let disp = locals.disp(local_idx);
    // Peephole: `x = x ± K` for int locals collapses to an in-place
    // memory op:
    //   K == 1: `inc/dec word ptr [bp-disp]`     (3 bytes)
    //   |K| ≤ 127, ±: `add/sub [bp-disp], imm8sx` (4 bytes)
    //   larger K, ±: `add/sub [bp-disp], imm16`   (5 bytes)
    // Pattern requires LHS = Local(this) on the BinOp.
    if locals.size(local_idx) == 2
        && let Expr::BinOp { op, left, right } = value
        && let Expr::Local(li) = left.as_ref()
        && *li == local_idx
        && matches!(op, BinOp::Add | BinOp::Sub)
        && let Some(k) = right.fold(locals.inits)
    {
        match (op, k) {
            (BinOp::Add, 1) => {
                out.extend_from_slice(&[0xFF, 0x46, disp as u8]);
                return;
            }
            (BinOp::Sub, 1) => {
                out.extend_from_slice(&[0xFF, 0x4E, disp as u8]);
                return;
            }
            _ => {
                // `83 /0 imm8sx` add or `83 /5 imm8sx` sub at [bp+disp8].
                let modrm_base = if matches!(op, BinOp::Add) { 0x46 } else { 0x6E };
                if let Ok(k8) = i8::try_from(k) {
                    out.extend_from_slice(&[0x83, modrm_base, disp as u8, k8 as u8]);
                } else {
                    let modrm_big = if matches!(op, BinOp::Add) { 0x46 } else { 0x6E };
                    out.extend_from_slice(&[0x81, modrm_big, disp as u8]);
                    let imm = (k as u32 & 0xFFFF) as u16;
                    out.extend_from_slice(&imm.to_le_bytes());
                }
                return;
            }
        }
    }
    // General path: evaluate the RHS into AX, then store.
    let is_byte = locals.size(local_idx) == 1;
    if let Some(k) = value.fold(locals.inits) {
        if is_byte {
            // `c6 46 disp imm8` — store low byte to char slot.
            out.push(0xC6);
            out.push(0x46);
            out.push(disp as u8);
            out.push((k as u32 & 0xFF) as u8);
        } else {
            let imm = (k as u32 & 0xFFFF) as u16;
            out.push(0xC7);
            out.push(0x46);
            out.push(disp as u8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if is_byte {
            // `88 46 disp` — store AL to char slot.
            out.push(0x88);
            out.push(0x46);
            out.push(disp as u8);
        } else {
            out.push(0x89);                       // MOV r/m16, r16
            out.push(0x46);
            out.push(disp as u8);
        }
    }
}

/// `<global> = <expr>;`. Constant RHS → `c7 06 addr imm16`
/// (mov word ptr [imm16], imm16, 6 bytes); general RHS →
/// `<expr-to-ax>; a3 addr` (mov moffs16, ax, 3 bytes).
/// Both shapes plant a 2-byte address placeholder that the linker
/// resolves via a GlobalAddr fixup.
fn emit_assign_global(global_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Long-global = long-global. Plain copy through DX:AX.
    //   `b = a` → `mov ax, [a]; mov dx, [a+2];
    //              mov [b], ax; mov [b+2], dx`
    if locals.is_long_global(global_idx)
        && let Expr::Global(src) = value
        && locals.is_long_global(*src)
    {
        let off = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *src } });
        out.push(0x8B);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *src } });
        out.push(0xA3);
        let off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        out.push(0x89);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    // Long-global = long-global ± long-global. Loads lhs into DX:AX,
    // applies a memory add/sub to both halves, stores to dest.
    //   `g = a + b` → `mov ax, [a]; mov dx, [a+2];
    //                  add ax, [b]; adc dx, [b+2];
    //                  mov [g], ax; mov [g+2], dx`
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Add | BinOp::Sub)
        && let Expr::Global(a) = left.as_ref()
        && let Expr::Global(b) = right.as_ref()
        && locals.is_long_global(*a)
        && locals.is_long_global(*b)
    {
        let (low_op, high_op): (u8, u8) = match op {
            BinOp::Add => (0x03, 0x13), // add r16,m16 / adc r16,m16
            BinOp::Sub => (0x2B, 0x1B), // sub r16,m16 / sbb r16,m16
            _ => unreachable!(),
        };
        // mov ax, [a]
        let off = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *a } });
        // mov dx, [a+2]
        out.push(0x8B);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *a } });
        // op ax, [b]
        out.push(low_op);
        out.push(0x06);
        let off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *b } });
        // op dx, [b+2]
        out.push(high_op);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *b } });
        // mov [g], ax
        out.push(0xA3);
        let off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        // mov [g+2], dx
        out.push(0x89);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    // Long-global compound add/sub by a foldable RHS:
    //   `g += K` → `add [g], K_lo; adc [g+2], K_hi`
    //   `g -= K` → `sub [g], K_lo; sbb [g+2], K_hi`
    // Falls through to the const-store path if the RHS isn't a
    // (Global(g) ± K) shape (fixture 1148).
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Add | BinOp::Sub)
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
        && let Some(k) = right.fold(locals.inits)
    {
        let low = (k as u32 & 0xFFFF) as i16;
        let high = ((k as i32) >> 16) as i16;
        let (sub_low_op, sub_high_op): (u8, u8) = match op {
            BinOp::Add => (0x00, 0x02), // /0 = add, /2 = adc
            BinOp::Sub => (0x05, 0x03), // /5 = sub, /3 = sbb
            _ => unreachable!(),
        };
        // Low-half: 83 06|2E low_imm8 (sx) or 81 06|2E low_imm16
        let low_fits_i8 = i8::try_from(low).is_ok();
        if low_fits_i8 {
            out.push(0x83);
            out.push(0x06 | (sub_low_op << 3));
            let addr_off = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            out.push(low as i8 as u8);
            fixups.push(Fixup {
                body_offset: addr_off - 1,
                kind: FixupKind::GlobalAddr { global_idx },
            });
        } else {
            out.push(0x81);
            out.push(0x06 | (sub_low_op << 3));
            let addr_off = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            out.extend_from_slice(&(low as u16).to_le_bytes());
            fixups.push(Fixup {
                body_offset: addr_off - 1,
                kind: FixupKind::GlobalAddr { global_idx },
            });
        }
        // High-half: 83 16 (or 1E) high_imm8 vs 81 16/1E ...
        let high_fits_i8 = i8::try_from(high).is_ok();
        if high_fits_i8 {
            out.push(0x83);
            out.push(0x06 | (sub_high_op << 3));
            let addr_off = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            out.push(high as i8 as u8);
            fixups.push(Fixup {
                body_offset: addr_off - 1,
                kind: FixupKind::GlobalAddr { global_idx },
            });
        } else {
            out.push(0x81);
            out.push(0x06 | (sub_high_op << 3));
            let addr_off = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            out.extend_from_slice(&(high as u16).to_le_bytes());
            fixups.push(Fixup {
                body_offset: addr_off - 1,
                kind: FixupKind::GlobalAddr { global_idx },
            });
        }
        return;
    }
    // Long globals get a special 4-byte store: low word at [g],
    // sign-extended high word at [g+2]. Only the constant-RHS shape
    // is wired up (most-common `long g = K;` pattern); a runtime RHS
    // would require DX:AX widening from the int RHS, deferred.
    if locals.is_long_global(global_idx)
        && let Some(k) = value.fold(locals.inits)
    {
        let low = (k as u32 & 0xFFFF) as u16;
        let high = (((k as i32) >> 31) as u32 & 0xFFFF) as u16;
        // c7 06 <addr> <low>
        out.push(0xC7);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&low.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
        // c7 06 <addr+2> <high>
        out.push(0xC7);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&high.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
        return;
    }
    // Peephole: `g = g ± 1;` → `inc/dec word ptr [g]` (4 bytes:
    // `ff /0 mem` for inc, `ff /1 mem` for dec). Fixture 4141.
    if let Expr::BinOp { op, left, right } = value
        && let Expr::Global(li) = left.as_ref()
        && *li == global_idx
        && let Expr::IntLit(1) = right.as_ref()
        && matches!(op, BinOp::Add | BinOp::Sub)
    {
        let modrm = if matches!(op, BinOp::Add) { 0x06 } else { 0x0E };
        out.push(0xFF);
        out.push(modrm);
        let body_offset = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
        return;
    }
    if let Some(k) = value.fold(locals.inits) {
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

/// `<global>[K] = <expr>;` — write at a constant array index. The
/// placeholder address is `byte_off`, which the linker adds to the
/// global's base. Constant RHS → `c7 06 byte_off imm16`; general RHS
/// → `<expr-to-ax>; a3 byte_off`. Fixture 4119.
fn emit_assign_indexed_global(global_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        out.extend_from_slice(&imm.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0xA3);
        let addr_off = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    }
}

/// `<char-global>[K] = <byte>;` — store one byte at a constant
/// index. `c6 06 byte_off imm8`. Fixture 4122.
fn emit_assign_indexed_global_byte(global_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let k = value.fold(locals.inits).unwrap_or_else(|| {
        panic!("non-constant char-array store value not yet supported")
    });
    let imm = (k as u32 & 0xFF) as u8;
    out.push(0xC6);
    out.push(0x06);
    let addr_off = out.len();
    out.extend_from_slice(&byte_off.to_le_bytes());
    out.push(imm);
    fixups.push(Fixup {
        body_offset: addr_off - 1,
        kind: FixupKind::GlobalAddr { global_idx },
    });
}

/// `<char-ptr-global>[K] = <byte>;` — load pointer into BX, then
/// `c6 47 disp imm8` (mov byte ptr [bx+disp], imm8). Fixture 4124.
fn emit_assign_ptr_index_byte(ptr_idx: usize, disp: i8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let k = value.fold(locals.inits).unwrap_or_else(|| {
        panic!("non-constant ptr-byte-store value not yet supported")
    });
    let imm = (k as u32 & 0xFF) as u8;
    let body_offset = out.len();
    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
    fixups.push(Fixup {
        body_offset: body_offset + 1,
        kind: FixupKind::GlobalAddr { global_idx: ptr_idx },
    });
    out.extend_from_slice(&[0xC6, 0x47, disp as u8, imm]);
}

/// `*<ptr-global> = <expr>;` — store through a pointer global.
/// Pattern: `mov bx, [p]` (load pointer) then store via `[bx]`.
/// Constant RHS uses `c7 07 imm16`; general RHS uses
/// `<expr-to-ax>; mov [bx], ax` (89 07). Fixture 4116.
/// `<local-int-array>[K] = <expr>;` — word store at element K.
/// Constant RHS → `c7 46 disp imm16` at `disp = locals.disp(local) +
/// byte_off`. Non-constant RHS → `<expr-to-ax>; 89 46 disp`.
fn emit_assign_indexed_local(local_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = locals.disp(local_idx) + byte_off as i16;
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x46);
        out.push(disp as u8);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0x89);
        out.push(0x46);
        out.push(disp as u8);
    }
}

/// `<local-char-array>[K] = <byte>;` — byte store at element K.
/// Constant RHS → `c6 46 disp imm8`; non-constant RHS evaluated
/// into AL via cbw-suppression. Fixture 1134.
fn emit_assign_indexed_local_byte(local_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, _out: &mut Vec<u8>, _fixups: &mut Vec<Fixup>) {
    let _ = locals.disp(local_idx) + byte_off as i16;
    let k = value.fold(locals.inits).unwrap_or_else(|| {
        panic!("non-constant char-local-array store value not yet supported")
    });
    let imm = (k as u32 & 0xFF) as u8;
    let disp = locals.disp(local_idx) + byte_off as i16;
    _out.push(0xC6);
    _out.push(0x46);
    _out.push(disp as u8);
    _out.push(imm);
}

/// `<struct-global>.<field> = <expr>;` — store at the global's
/// address + byte_off. Word: `c7 06 disp imm16`; byte: `c6 06 disp imm8`.
fn emit_assign_global_field(global_idx: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte global-struct-field store not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        out.push(0xC6);
        out.push(0x06);
        let body_offset = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        out.push(imm);
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x06);
        let body_offset = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        out.extend_from_slice(&imm.to_le_bytes());
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0xA3);
        let body_offset = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    }
}

/// `<struct-local>.<field> = <expr>;` — store at `disp + byte_off`,
/// picking word vs byte form based on the field's size.
fn emit_assign_local_field(local_idx: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = locals.disp(local_idx) + byte_off as i16;
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte struct-field store not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        out.push(0xC6);
        out.push(0x46);
        out.push(disp as u8);
        out.push(imm);
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x46);
        out.push(disp as u8);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0x89);
        out.push(0x46);
        out.push(disp as u8);
    }
}

/// `<struct-ptr-param>-><field> = <expr>;` — same shape as the
/// local-pointer version but the BX-load disp is positive (param).
fn emit_assign_deref_param_field(ptr_param: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let p_disp = param_disp(ptr_param);
    out.push(0x8B);
    out.push(0x5E);
    out.push(p_disp as u8);
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte struct-field store via param ptr not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        if byte_off == 0 {
            out.extend_from_slice(&[0xC6, 0x07, imm]);
        } else {
            out.push(0xC6);
            out.push(0x47);
            out.push(byte_off as u8);
            out.push(imm);
        }
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        if byte_off == 0 {
            out.push(0xC7);
            out.push(0x07);
        } else {
            out.push(0xC7);
            out.push(0x47);
            out.push(byte_off as u8);
        }
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if byte_off == 0 {
            out.extend_from_slice(&[0x89, 0x07]);
        } else {
            out.push(0x89);
            out.push(0x47);
            out.push(byte_off as u8);
        }
    }
}

/// `<struct-ptr-local>-><field> = <expr>;` — `mov bx, [bp+disp];
/// c7 47 byte_off imm16` (or byte form).
fn emit_assign_deref_local_field(ptr_local: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let p_disp = locals.disp(ptr_local);
    out.push(0x8B);
    out.push(0x5E);
    out.push(p_disp as u8);
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte struct-field store via ptr not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        if byte_off == 0 {
            out.extend_from_slice(&[0xC6, 0x07, imm]);
        } else {
            out.push(0xC6);
            out.push(0x47);
            out.push(byte_off as u8);
            out.push(imm);
        }
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        if byte_off == 0 {
            out.push(0xC7);
            out.push(0x07);
        } else {
            out.push(0xC7);
            out.push(0x47);
            out.push(byte_off as u8);
        }
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if byte_off == 0 {
            out.extend_from_slice(&[0x89, 0x07]);
        } else {
            out.push(0x89);
            out.push(0x47);
            out.push(byte_off as u8);
        }
    }
}

/// `*<ptr-local> = <expr>;` — load the pointer local into BX, then
/// store the RHS through `[bx]`. Constant RHS → `c7 07 imm16`;
/// general RHS → `<expr-to-ax>; 89 07`.
fn emit_assign_deref_local(local_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = locals.disp(local_idx);
    // `mov bx, [bp-disp]` — 8b 5e disp8.
    out.extend_from_slice(&[0x8B, 0x5E, disp as u8]);
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.extend_from_slice(&[0xC7, 0x07]);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.extend_from_slice(&[0x89, 0x07]);
    }
}

fn emit_assign_deref_global(global_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // `mov bx, [p]` — load pointer global into BX.
    let body_offset = out.len();
    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
    fixups.push(Fixup {
        body_offset: body_offset + 1,
        kind: FixupKind::GlobalAddr { global_idx },
    });
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        // `c7 07 imm16` — mov word ptr [bx], imm16.
        out.extend_from_slice(&[0xC7, 0x07]);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        // `89 07` — mov [bx], ax.
        out.extend_from_slice(&[0x89, 0x07]);
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
    locals: &Locals<'_>,
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
    locals: &Locals<'_>,
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
    locals: &Locals<'_>,
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
    let mut cmp_fixups: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond, locals, &mut cmp_buf, &mut cmp_fixups);

    // Alignment: position right after the 2-byte `eb XX` should be
    // even. If it would be odd, insert a NOP pad and bump the
    // forward jmp displacement by 1.
    let pos_after_jmp = out.len() + 2;
    let needs_pad = pos_after_jmp % 2 != 0;
    let pad = if needs_pad { 1 } else { 0 };

    let jcc_opcode = match cond {
        Cond::Truthy(_) => 0x75,             // jne (back when nonzero)
        Cond::Cmp { op, .. } => loop_back_jcc(*op),
        Cond::And(_, _) | Cond::Or(_, _) => {
            panic!("&&/|| in while/do-while not yet supported");
        }
    };

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
    let cmp_base = out.len();
    out.extend_from_slice(&cmp_buf);
    for mut c in cmp_fixups {
        c.body_offset += cmp_base;
        fixups.push(c);
    }
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
    locals: &Locals<'_>,
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
    let mut cmp_fixups: Vec<Fixup> = Vec::new();
    if !elide_cmp {
        emit_cond_cmp(cond, locals, &mut cmp_buf, &mut cmp_fixups);
    }
    let cmp_len = cmp_buf.len();
    let jcc_opcode = match cond {
        Cond::Truthy(_) => 0x75,             // jne (back when nonzero)
        Cond::Cmp { op, .. } => loop_back_jcc(*op),
        Cond::And(_, _) | Cond::Or(_, _) => {
            panic!("&&/|| in while/do-while not yet supported");
        }
    };
    let body_base = out.len();
    out.extend_from_slice(&body_buf);
    for mut c in body_fixups {
        c.body_offset += body_base;
        fixups.push(c);
    }
    let cmp_base = out.len();
    out.extend_from_slice(&cmp_buf);
    for mut c in cmp_fixups {
        c.body_offset += cmp_base;
        fixups.push(c);
    }
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
fn emit_cond_cmp(cond: &Cond, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    emit_cond_cmp_inner(cond, locals, out, fixups);
}

/// Emit `cmp <X>, <Y>; <inverted-jcc> skip` where `skip` is a
/// forward `rel8` displacement equal to `take_then_disp`. The
/// caller has pre-computed the size of the then-body so we can use
/// the 2-byte jcc form without a fixup. Fixtures 4090 / 4091 / 4092.
fn emit_cond_skip(cond: &Cond, take_then_disp: i8, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match cond {
        Cond::And(a, b) => {
            // Pre-emit `b`'s cmp+jcc-to-skip into a scratch buffer
            // so we know its size. `a`'s skip target jumps over `b`
            // AND the body. `b`'s skip target jumps over just the
            // body — the original `take_then_disp`.
            let mut b_buf = Vec::new();
            let mut b_fixups = Vec::new();
            emit_cond_skip(b, take_then_disp, locals, &mut b_buf, &mut b_fixups);
            let a_skip = i8::try_from(b_buf.len() as i32 + take_then_disp as i32)
                .expect("&&-cond skip fits in rel8");
            emit_cond_skip(a, a_skip, locals, out, fixups);
            let b_base = out.len();
            out.extend_from_slice(&b_buf);
            for mut f in b_fixups {
                f.body_offset += b_base;
                fixups.push(f);
            }
        }
        Cond::Or(a, b) => {
            // `a` true → jump into the body (skipping `b`). `a`
            // false → fall through to `b`. `b` evaluates as a
            // normal skip-cond: true → fall into body, false → skip.
            //
            // For `a`'s "take then" direction we need to invert the
            // jcc to a "take when true" form and target the start of
            // the body. We use emit_cond_take helper which emits
            // cmp + take-then-jcc instead of skip-jcc.
            let mut b_buf = Vec::new();
            let mut b_fixups = Vec::new();
            emit_cond_skip(b, take_then_disp, locals, &mut b_buf, &mut b_fixups);
            // a's "take" disp: jump past b's emission to the body.
            let a_take = i8::try_from(b_buf.len() as i32)
                .expect("||-cond take disp fits in rel8");
            emit_cond_take(a, a_take, locals, out, fixups);
            let b_base = out.len();
            out.extend_from_slice(&b_buf);
            for mut f in b_fixups {
                f.body_offset += b_base;
                fixups.push(f);
            }
        }
        _ => {
            let jcc = match cond {
                Cond::Truthy(_) => 0x74, // je on zero
                Cond::Cmp { op, .. } => inverted_jcc(*op),
                _ => unreachable!(),
            };
            emit_cond_cmp_inner(cond, locals, out, fixups);
            out.push(jcc);
            out.push(take_then_disp as u8);
        }
    }
}

/// Counterpart of `emit_cond_skip` — emits cmp + a jcc that fires
/// when the condition is *true*, skipping ahead by `take_disp`
/// bytes. Used by Cond::Or short-circuit so `a` jumps into the body
/// when satisfied.
fn emit_cond_take(cond: &Cond, take_disp: i8, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match cond {
        Cond::And(_, _) | Cond::Or(_, _) => {
            panic!("nested &&/|| in emit_cond_take not yet supported");
        }
        _ => {
            let jcc = match cond {
                Cond::Truthy(_) => 0x75, // jne (take when nonzero)
                Cond::Cmp { op, .. } => loop_back_jcc(*op),
                _ => unreachable!(),
            };
            emit_cond_cmp_inner(cond, locals, out, fixups);
            out.push(jcc);
            out.push(take_disp as u8);
        }
    }
}

/// Emit only the cmp half of a Cond. Used by both emit_cond_skip
/// (forward jcc for `if`) and emit_cond_cmp (backward jcc for loops).
fn emit_cond_cmp_inner(cond: &Cond, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Long-global vs zero idiom: `mov ax, [g]; or ax, [g+2]` — ZF set
    // iff both halves are zero. Covers `if (g == 0)`, `if (!g)`, and
    // (with inverted jcc) `if (g != 0)`.
    if let Some(idx) = match cond {
        Cond::Truthy(Expr::Global(idx)) if locals.is_long_global(*idx) => Some(*idx),
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::Global(idx), right: Expr::IntLit(0) }
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::IntLit(0), right: Expr::Global(idx) }
            if locals.is_long_global(*idx) => Some(*idx),
        _ => None,
    } {
        // mov ax, [g]
        let off = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: idx } });
        // or ax, [g+2]  →  0b 06 <off+2>
        out.push(0x0B);
        out.push(0x06);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: idx } });
        return;
    }
    match cond {
        Cond::Truthy(Expr::Local(idx)) => emit_cmp_local_imm(*idx, locals, 0, out),
        Cond::Truthy(Expr::Param(idx)) => emit_cmp_bp_imm(param_disp(*idx), 0, out),
        Cond::Truthy(Expr::Global(idx)) => emit_cmp_global_imm(*idx, 0, out, fixups),
        Cond::Cmp { op: _, left: Expr::Local(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Local(idx) } => {
            emit_cmp_local_imm(*idx, locals, *k, out);
        }
        Cond::Cmp { op: _, left: Expr::Param(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Param(idx) } => {
            emit_cmp_bp_imm(param_disp(*idx), *k, out);
        }
        Cond::Cmp { op: _, left: Expr::Local(li), right: Expr::Param(pi) } => {
            // `cmp <local>, <param>` → `mov ax, [bp-li]; cmp ax, [bp+pi]`.
            emit_load_local(*li, locals, out);
            let p_disp = param_disp(*pi);
            out.push(0x3B);
            out.push(0x46);
            out.push(p_disp as u8);
        }
        Cond::Cmp { op: _, left: Expr::Local(li), right: Expr::Local(rj) } => {
            // `cmp <local>, <local>` → load LHS, cmp [bp-rj].
            emit_load_local(*li, locals, out);
            let r_disp = locals.disp(*rj);
            out.push(0x3B);
            out.push(0x46);
            out.push(r_disp as u8);
        }
        Cond::Cmp { op: _, left: Expr::Global(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Global(idx) } => {
            emit_cmp_global_imm(*idx, *k, out, fixups);
        }
        // Generic fallback for unsupported cond shapes: evaluate LHS
        // into AX, then `cmp ax, K` (`3d K K` for word, `83 f8 K` for
        // imm8sx). Used for `p->x == K` and other DerefLocalField cases.
        Cond::Cmp { op: _, left, right: Expr::IntLit(k) } => {
            emit_expr_to_ax(left, locals, out, fixups);
            emit_cmp_ax_imm(*k, out);
        }
        Cond::Cmp { op: _, left: Expr::IntLit(k), right } => {
            emit_expr_to_ax(right, locals, out, fixups);
            emit_cmp_ax_imm(*k, out);
        }
        Cond::Truthy(expr) => {
            // `if (<expr>)` — evaluate to AX, then test ax, ax (or
            // cmp ax, 0). MSC picks `or ax, ax` (`0b c0`) as the
            // 2-byte test. Most truthy-on-local/global cases were
            // handled earlier; this is the fallback.
            emit_expr_to_ax(expr, locals, out, fixups);
            out.extend_from_slice(&[0x0B, 0xC0]);
        }
        other => panic!("Slice 5 cond shape not yet supported: {other:?}"),
    }
}

/// `cmp ax, imm` — picks `83 f8 imm8sx` for small constants,
/// `3d imm16` for larger.
fn emit_cmp_ax_imm(k: i32, out: &mut Vec<u8>) {
    if let Ok(k8) = i8::try_from(k) {
        out.push(0x83);
        out.push(0xF8);
        out.push(k8 as u8);
    } else {
        let k16 = (k as u32 & 0xFFFF) as u16;
        out.push(0x3D);
        out.extend_from_slice(&k16.to_le_bytes());
    }
}

fn param_disp(idx: usize) -> i16 {
    i16::try_from(4 + (idx * 2)).expect("param disp fits in i16")
}

/// The signed conditional-jump opcode for **falling through to the
/// then-block** when `op` is satisfied. Used by `emit_cond_skip`: the
/// emitted `jcc skip` triggers when the cond is *false*, so this is
/// the **inversion** of the source-level relop. MSC's signed-jcc
/// mnemonic family is `7c..7f` for the disp8 forms.
fn inverted_jcc(op: RelOp) -> u8 {
    match op {
        RelOp::Eq => 0x75, // jne — skip then-block when not-equal
        RelOp::Ne => 0x74, // je
        RelOp::Lt => 0x7D, // jge — signed
        RelOp::Le => 0x7F, // jg
        RelOp::Gt => 0x7E, // jle
        RelOp::Ge => 0x7C, // jl
    }
}

/// Loop-back jcc opcode — fires when cond is *true*, so it's NOT
/// inverted. Used by `emit_while` / `emit_do_while` for the tail
/// branch back to the loop top.
fn loop_back_jcc(op: RelOp) -> u8 {
    match op {
        RelOp::Eq => 0x74, // je
        RelOp::Ne => 0x75, // jne
        RelOp::Lt => 0x7C, // jl
        RelOp::Le => 0x7E, // jle
        RelOp::Gt => 0x7F, // jg
        RelOp::Ge => 0x7D, // jge
    }
}

/// `cmp word ptr [<global-addr>], imm8` — MSC picks the `83 3e disp imm8`
/// form for global compares against a sign-extended byte (fixtures
/// 4129, 4133). The placeholder address gets a GlobalAddr FIXUP.
fn emit_cmp_global_imm(global_idx: usize, k: i32, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let k_i8 = i8::try_from(k).expect("global cmp imm fits in i8");
    out.push(0x83);
    out.push(0x3E);
    let body_offset = out.len();
    out.extend_from_slice(&[0x00, 0x00, k_i8 as u8]);
    fixups.push(Fixup {
        body_offset: body_offset - 1,
        kind: FixupKind::GlobalAddr { global_idx },
    });
}

/// `cmp word ptr [bp-disp], imm` — MSC picks the `83 /7 r/m imm8sx`
/// form when the immediate fits in a sign-extended byte (which is
/// every fixture exercised by Slice 5 today). The 5-byte
/// `81 7e disp imm16` form is reserved for larger constants.
fn emit_cmp_local_imm(idx: usize, locals: &Locals<'_>, k: i32, out: &mut Vec<u8>) {
    emit_cmp_bp_imm(locals.disp(idx), k, out);
}

/// `cmp word ptr [bp+disp], imm` — covers both local (negative disp)
/// and param (positive disp) compares. Picks `83 /7 r/m imm8sx` for
/// the small-immediate form, `81 /7 r/m imm16` for the wide form.
fn emit_cmp_bp_imm(disp: i16, k: i32, out: &mut Vec<u8>) {
    if let Ok(k_i8) = i8::try_from(k) {
        out.push(0x83);
        out.push(0x7E);          // ModR/M: mod=01 reg=111 (/7=CMP) r/m=110 (BP+disp8)
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
fn emit_expr_to_ax(expr: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match expr {
        Expr::IntLit(k) => {
            // Foldable path is handled by the caller; this arm only
            // fires if the caller bypassed folding.
            let imm = (*k as u32 & 0xFFFF) as u16;
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Expr::Local(i) => {
            emit_load_local(*i, locals, out);
        }
        Expr::Param(i) => {
            emit_load_param(*i, out);
        }
        Expr::BinOp { op, left, right } => {
            emit_binop(*op, left, right, locals, out, fixups);
        }
        Expr::Call { name, args } => {
            // Result lands in AX. Caller consumes from there.
            // Fixture 1220 / 1283 / 1286 — call as part of an
            // arithmetic or assignment expression.
            emit_call(name, args, locals, out, fixups);
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
        Expr::DerefByte { ptr } => {
            // Phase 1: `*<char-ptr-global>` and `*(<char-ptr> + K)`.
            // Both lower to `mov bx, [p]; mov al, [bx+disp]; cbw`,
            // with disp 0 for the bare deref and K for the
            // constant-offset form (fixtures 4111, 4127).
            let (ptr_idx, disp) = match ptr.as_ref() {
                Expr::Global(idx) => (*idx, 0i8),
                Expr::BinOp { op: BinOp::Add, left, right }
                | Expr::BinOp { op: BinOp::Add, left: right, right: left }
                    if matches!(left.as_ref(), Expr::Global(_))
                    && matches!(right.as_ref(), Expr::IntLit(_)) =>
                {
                    let Expr::Global(idx) = **left else { unreachable!() };
                    let Expr::IntLit(k) = **right else { unreachable!() };
                    (idx, i8::try_from(k).expect("ptr-add offset fits in i8"))
                }
                other => panic!("byte deref of {other:?} not yet supported"),
            };
            let body_offset = out.len();
            out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset + 1,
                kind: FixupKind::GlobalAddr { global_idx: ptr_idx },
            });
            if disp == 0 {
                out.extend_from_slice(&[0x8A, 0x07, 0x98]);
            } else {
                out.extend_from_slice(&[0x8A, 0x47, disp as u8, 0x98]);
            }
        }
        Expr::DerefWord { ptr } => {
            // Phase 1: `*<int-ptr-param>` (fixture 4125). Pattern is
            // `mov bx, [bp+disp]; mov ax, [bx]`. Future fixtures with
            // a global int-pointer source pick up the BX-from-global
            // load shape.
            match ptr.as_ref() {
                Expr::Param(i) => {
                    let disp = i8::try_from(4 + (*i * 2))
                        .expect("param disp fits");
                    out.extend_from_slice(&[0x8B, 0x5E, disp as u8]);
                    out.extend_from_slice(&[0x8B, 0x07]);
                }
                Expr::Global(idx) => {
                    let body_offset = out.len();
                    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
                    fixups.push(Fixup {
                        body_offset: body_offset + 1,
                        kind: FixupKind::GlobalAddr { global_idx: *idx },
                    });
                    out.extend_from_slice(&[0x8B, 0x07]);
                }
                other => panic!("word deref of {other:?} not yet supported"),
            }
        }
        Expr::PtrIndexByte { ptr, index } => {
            // Constant index: load pointer global into BX, then
            // `mov al, [bx + disp]` and `cbw`. `disp` is the byte
            // index. Fixture 4123. Phase 1 keeps it disp8.
            let k = index.fold(locals.inits).unwrap_or_else(|| {
                panic!("non-constant char-ptr index not yet supported")
            });
            let disp = i8::try_from(k).expect("ptr index fits in i8");
            let body_offset = out.len();
            out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset + 1,
                kind: FixupKind::GlobalAddr { global_idx: *ptr },
            });
            out.extend_from_slice(&[0x8A, 0x47, disp as u8, 0x98]);
        }
        Expr::IndexByte { array, index } => {
            // Phase 1: constant index only — `a0 <byte_off> 98`.
            // The placeholder is the index itself (size 1 per slot);
            // the linker adds the array's base address.
            let k = index.fold(locals.inits).unwrap_or_else(|| {
                panic!("non-constant char-array index not yet supported")
            });
            let byte_off = (k as u32 & 0xFFFF) as u16;
            let body_offset = out.len();
            out.push(0xA0);
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *array },
            });
            out.push(0x98);
        }
        Expr::Index { array, index } => {
            if let Some(k) = index.fold(locals.inits) {
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
        Expr::AddrOfGlobal(idx) => {
            let body_offset = out.len();
            out.extend_from_slice(&[0xB8, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *idx },
            });
        }
        Expr::AddrOfLocal(idx) => {
            // `lea ax, [bp-disp]` = `8d 46 disp`.
            let disp = locals.disp(*idx);
            out.extend_from_slice(&[0x8D, 0x46, disp as u8]);
        }
        Expr::Ternary { .. } => {
            // Should have folded already by the caller for runtime
            // ternary codegen we don't yet implement.
            panic!("non-constant ternary not yet supported");
        }
        Expr::GlobalField { global, byte_off, size } => {
            // Word field: `a1 byte_off byte_off` + GlobalAddr FIXUP.
            // Byte field: `a0 byte_off byte_off; 98` + FIXUP.
            let body_offset = out.len();
            if *size == 1 {
                out.push(0xA0);
            } else {
                out.push(0xA1);
            }
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *global },
            });
            if *size == 1 {
                out.push(0x98);
            }
        }
        Expr::LocalField { local, byte_off, size } => {
            let disp = locals.disp(*local) + *byte_off as i16;
            if *size == 1 {
                out.push(0x8A);
                out.push(0x46);
                out.push(disp as u8);
                out.push(0x98); // cbw
            } else {
                out.push(0x8B);
                out.push(0x46);
                out.push(disp as u8);
            }
        }
        Expr::DerefParamField { ptr_param, byte_off, size } => {
            let p_disp = param_disp(*ptr_param);
            out.push(0x8B);
            out.push(0x5E);
            out.push(p_disp as u8);
            if *size == 1 {
                if *byte_off == 0 {
                    out.extend_from_slice(&[0x8A, 0x07]);
                } else {
                    out.push(0x8A);
                    out.push(0x47);
                    out.push(*byte_off as u8);
                }
                out.push(0x98);
            } else if *byte_off == 0 {
                out.extend_from_slice(&[0x8B, 0x07]);
            } else {
                out.push(0x8B);
                out.push(0x47);
                out.push(*byte_off as u8);
            }
        }
        Expr::DerefLocalField { ptr_local, byte_off, size } => {
            // mov bx, [bp + ptr_disp]; mov ax, [bx + byte_off] (word)
            //                          or mov al, [bx + byte_off]; cbw (byte)
            let p_disp = locals.disp(*ptr_local);
            out.push(0x8B);
            out.push(0x5E);
            out.push(p_disp as u8);
            if *size == 1 {
                if *byte_off == 0 {
                    out.extend_from_slice(&[0x8A, 0x07]);
                } else {
                    out.push(0x8A);
                    out.push(0x47);
                    out.push(*byte_off as u8);
                }
                out.push(0x98); // cbw
            } else if *byte_off == 0 {
                out.extend_from_slice(&[0x8B, 0x07]);
            } else {
                out.push(0x8B);
                out.push(0x47);
                out.push(*byte_off as u8);
            }
        }
        Expr::ParamIndex { param, index } => {
            // Constant K → `mov bx, [bp+param_disp]; mov ax, [bx+2K]`.
            // Fixture 1236.
            let k = index.fold(locals.inits).unwrap_or_else(|| {
                panic!("non-constant param-index not yet supported")
            });
            let p_disp = param_disp(*param);
            out.push(0x8B);
            out.push(0x5E);
            out.push(p_disp as u8);
            let elem_disp = (k as i16) * 2;
            if elem_disp == 0 {
                out.extend_from_slice(&[0x8B, 0x07]);
            } else {
                out.push(0x8B);
                out.push(0x47);
                out.push(elem_disp as u8);
            }
        }
        Expr::LocalIndex { local, index } => {
            // Constant K → `mov ax, [bp - disp + 2K]` (`8b 46 disp`).
            let k = index.fold(locals.inits).unwrap_or_else(|| {
                panic!("non-constant local-int-array index not yet supported")
            });
            let disp = locals.disp(*local) + (k as i16) * 2;
            out.push(0x8B);
            out.push(0x46);
            out.push(disp as u8);
        }
        Expr::LocalIndexByte { local, index } => {
            // Constant K → `mov al, [bp - disp + K]; cbw` (`8a 46 disp; 98`).
            let k = index.fold(locals.inits).unwrap_or_else(|| {
                panic!("non-constant local-char-array index not yet supported")
            });
            let disp = locals.disp(*local) + (k as i16);
            out.push(0x8A);
            out.push(0x46);
            out.push(disp as u8);
            out.push(0x98);
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

/// `mov ax, word ptr [bp-disp]` — 3-byte form `8B 46 disp8`. The
/// caller has already converted the local index into a BP-relative
/// displacement (via `Locals::disp`).
fn emit_load_local(idx: usize, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let disp = locals.disp(idx);
    if locals.size(idx) == 1 {
        // `mov al, [bp-disp]; cbw` — sign-extend char to int in AX.
        out.push(0x8A);
        out.push(0x46);
        out.push(disp as u8);
        out.push(0x98); // cbw
    } else {
        out.push(0x8B);
        out.push(0x46);
        out.push(disp as u8);
    }
}

fn emit_binop(op: BinOp, left: &Expr, right: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Commutative-op swap: when the RHS is a value that needs AX
    // (call, complex expression) and the LHS is a BP-rel operand,
    // MSC emits the RHS first and then uses the BP-rel as a memory
    // operand. Matches the factorial idiom `n * fact(n-1)` → call,
    // then `imul [bp+n_disp]`. Fixtures 1220, 1264, 1277.
    let commutative = matches!(op, BinOp::Add | BinOp::Mul | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor);
    if commutative
        && matches!(right, Expr::Call { .. })
        && let Some(disp) = bp_disp(left, locals)
    {
        emit_expr_to_ax(right, locals, out, fixups);
        emit_mem_op_at(op, disp, out);
        return;
    }
    // Left as a BP-rel operand we can load into AX.
    if let Some(load) = bp_load(left, locals) {
        load(out);
        // Right as IntLit → imm form.
        if let Expr::IntLit(k) = right {
            emit_imm_op(op, *k, out);
            return;
        }
        // Right as BP-rel → `op ax, [bp+disp]` mem form.
        if let Some(disp) = bp_disp(right, locals) {
            emit_mem_op_at(op, disp, out);
            return;
        }
    }
    // Left as a global → `mov ax, [g]; <op> ax, ...`. The RHS can
    // be an int literal (fixture 4135), another global via memory
    // operand (`03 06 addr`, fixture 4138), or a local/param via the
    // BP-rel path.
    if let Expr::Global(idx) = left {
        let body_offset = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup {
            body_offset,
            kind: FixupKind::GlobalAddr { global_idx: *idx },
        });
        if let Expr::IntLit(k) = right {
            emit_imm_op(op, *k, out);
            return;
        }
        if let Expr::Global(idx2) = right {
            // `op ax, word ptr [g2]` — Grp1 r/m16 with mod=00 r/m=110.
            let opcode = match op {
                BinOp::Add => 0x03,
                BinOp::Sub => 0x2B,
                BinOp::Mul => panic!("mul of two globals not yet supported"),
                _ => panic!("{op:?} between two globals not yet supported"),
            };
            out.push(opcode);
            out.push(0x06);
            let body_offset = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset - 1,
                kind: FixupKind::GlobalAddr { global_idx: *idx2 },
            });
            return;
        }
    }
    // Foldable side — recurse with the folded literal substituted.
    // Lets `(2 + x)` collapse to `(<lit> + <local>)` etc.
    if let Some(k) = left.fold(locals.inits) {
        emit_binop(op, &Expr::IntLit(k), right, locals, out, fixups);
        return;
    }
    if let Some(k) = right.fold(locals.inits) {
        emit_binop(op, left, &Expr::IntLit(k), locals, out, fixups);
        return;
    }
    // `a[I] + a[J]` where both ParamIndex share the same param:
    // load the pointer into BX once, then read RHS into AX and add
    // LHS as a memory operand. Matches MSC's `mov bx, [bp+disp];
    // mov ax, [bx+J*2]; add ax, [bx+I*2]` shape on fixture 1236.
    if let Expr::ParamIndex { param: lp, index: li } = left
        && let Expr::ParamIndex { param: rp, index: ri } = right
        && *lp == *rp
        && let Some(lk) = li.fold(locals.inits)
        && let Some(rk) = ri.fold(locals.inits)
        && matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
    {
        let p_disp = param_disp(*lp);
        out.push(0x8B);
        out.push(0x5E);
        out.push(p_disp as u8);
        // RHS first (matches MSC observation on 1236).
        let r_disp = (rk as i16) * 2;
        if r_disp == 0 {
            out.extend_from_slice(&[0x8B, 0x07]);
        } else {
            out.push(0x8B);
            out.push(0x47);
            out.push(r_disp as u8);
        }
        let opcode = match op {
            BinOp::Add => 0x03,
            BinOp::Sub => 0x2B,
            BinOp::BitAnd => 0x23,
            BinOp::BitOr => 0x0B,
            BinOp::BitXor => 0x33,
            _ => unreachable!(),
        };
        let l_disp = (lk as i16) * 2;
        if l_disp == 0 {
            out.push(opcode);
            out.push(0x07);
        } else {
            out.push(opcode);
            out.push(0x47);
            out.push(l_disp as u8);
        }
        return;
    }
    // Nested binop on the left (`(a + b) + c`, `g[0] + g[1] + g[2]`):
    // compute the left subtree into AX, then fold the right side in.
    // Fixtures 4139 / 1100.
    if let Expr::BinOp { .. } = left {
        emit_expr_to_ax(left, locals, out, fixups);
        return emit_binop_right(op, right, locals, out, fixups);
    }
    // Left as a global-array Index with constant K: synthesize a
    // global-load (`a1 byte_off`) and let the right side fold.
    if let Some((array_idx, byte_off)) = const_index_global(left, locals) {
        let body_offset = out.len();
        out.push(0xA1);
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup {
            body_offset,
            kind: FixupKind::GlobalAddr { global_idx: array_idx },
        });
        return emit_binop_right(op, right, locals, out, fixups);
    }
    panic!("binop shape not yet supported: {op:?} of {left:?}, {right:?}");
}

/// After the LHS is in AX, fold the RHS into the same register via
/// the appropriate `op ax, <operand>` form. Used by `emit_binop`
/// once it's resolved the LHS into AX.
fn emit_binop_right(op: BinOp, right: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Expr::IntLit(k) = right {
        emit_imm_op(op, *k, out);
        return;
    }
    if let Expr::Global(idx) = right {
        let opcode = match op {
            BinOp::Add => 0x03,
            BinOp::Sub => 0x2B,
            _ => panic!("{op:?} with global rhs not yet supported"),
        };
        out.push(opcode);
        out.push(0x06);
        let body_offset = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx: *idx },
        });
        return;
    }
    if let Some((array_idx, byte_off)) = const_index_global(right, locals) {
        let opcode = match op {
            BinOp::Add => 0x03,
            BinOp::Sub => 0x2B,
            _ => panic!("{op:?} with indexed-global rhs not yet supported"),
        };
        out.push(opcode);
        out.push(0x06);
        let body_offset = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx: array_idx },
        });
        return;
    }
    if let Some(disp) = bp_disp(right, locals) {
        emit_mem_op_at(op, disp, out);
        return;
    }
    panic!("binop_right shape not yet supported: {op:?} of {right:?}");
}

/// If `e` is an `Index { array, IntLit(K) }` (or the IndexByte
/// variant), return (array_idx, byte_off). Used by emit_binop to
/// promote `g[K]` to an `[addr]` operand directly.
fn const_index_global(e: &Expr, locals: &Locals<'_>) -> Option<(usize, u16)> {
    match e {
        Expr::Index { array, index } => {
            let k = index.fold(locals.inits)?;
            let off = u16::try_from((k as i64) * 2).ok()?;
            Some((*array, off))
        }
        Expr::IndexByte { array, index } => {
            let k = index.fold(locals.inits)?;
            let off = u16::try_from(k as i64).ok()?;
            Some((*array, off))
        }
        Expr::GlobalField { global, byte_off, size: 2 } => {
            Some((*global, *byte_off))
        }
        _ => None,
    }
}

/// If `e` is a Local or Param, return a closure that emits the
/// `mov ax, [bp+disp]` load. Otherwise return None. Used by
/// `emit_binop` to handle either operand kind on the left-hand side.
fn bp_load<'a>(e: &'a Expr, locals: &'a Locals<'_>) -> Option<Box<dyn FnOnce(&mut Vec<u8>) + 'a>> {
    match e {
        Expr::Local(i) => Some(Box::new(move |out: &mut Vec<u8>| emit_load_local(*i, locals, out))),
        Expr::Param(i) => Some(Box::new(move |out: &mut Vec<u8>| emit_load_param(*i, out))),
        _ => None,
    }
}

/// If `e` is a Local or Param, return its bp-relative byte
/// displacement (negative for locals, positive for params).
fn bp_disp(e: &Expr, locals: &Locals<'_>) -> Option<i16> {
    match e {
        Expr::Local(i) => Some(locals.disp(*i)),
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
        // Bitwise imm-AX: prefer 3-byte `83 /digit imm8sx` for small K,
        // 3-byte `op_byte imm16` (form `25/0d/35`) otherwise.
        (BinOp::BitAnd, _) => {
            if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, 0xE0, k8 as u8]);
            } else {
                out.push(0x25);
                out.extend_from_slice(&k16.to_le_bytes());
            }
        }
        (BinOp::BitOr, _) => {
            if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, 0xC8, k8 as u8]);
            } else {
                out.push(0x0D);
                out.extend_from_slice(&k16.to_le_bytes());
            }
        }
        (BinOp::BitXor, 0xFFFF) | (BinOp::BitXor, _) if k16 == 0xFFFF => {
            // `xor ax, -1` → `not ax` (f7 d0). 2 bytes vs 3. Fixture 1120.
            out.extend_from_slice(&[0xF7, 0xD0]);
        }
        (BinOp::BitXor, _) => {
            if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, 0xF0, k8 as u8]);
            } else {
                out.push(0x35);
                out.extend_from_slice(&k16.to_le_bytes());
            }
        }
        // 8086 shifts by constant: 1 uses `d1 e0/e8`; larger picks
        // `mov cl, K; shl/shr ax, cl` (4 bytes).
        (BinOp::Shl, 1) => out.extend_from_slice(&[0xD1, 0xE0]),
        (BinOp::Shr, 1) => out.extend_from_slice(&[0xD1, 0xE8]),
        (BinOp::Shl, _) => out.extend_from_slice(&[0xB1, k as u8, 0xD3, 0xE0]),
        (BinOp::Shr, _) => out.extend_from_slice(&[0xB1, k as u8, 0xD3, 0xE8]),
        (op, _) => {
            panic!("imm-op `{op:?}` not yet covered by a fixture (k={k})");
        }
    }
}

/// Per-operator emit for `<reg-AX> <op> word ptr [bp+disp]`. The
/// opcode-prefix byte for memory-source forms: 03=ADD, 2B=SUB.
/// Works for both negative disps (locals) and positive disps
/// (params); fixture 4102 uses param shape.
fn emit_mem_op_at(op: BinOp, disp: i16, out: &mut Vec<u8>) {
    let disp8 = i8::try_from(disp).expect("disp fits in i8");
    if matches!(op, BinOp::Mul) {
        // `imul word ptr [bp+disp8]` — `F7 /5 r/m`.
        out.push(0xF7);
        out.push(0x6E);  // mod=01 reg=101 (/5=IMUL) r/m=110 (BP-rel)
        out.push(disp8 as u8);
        return;
    }
    let opcode = match op {
        BinOp::Add => 0x03,
        BinOp::Sub => 0x2B,
        BinOp::BitAnd => 0x23,
        BinOp::BitOr => 0x0B,
        BinOp::BitXor => 0x33,
        _ => panic!("memory-source {op:?} not yet covered by a fixture"),
    };
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
