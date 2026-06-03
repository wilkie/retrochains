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

mod lex;
mod parse;
mod codegen;

// Phase modules hold the free functions; types/impls stay here.
pub(crate) use lex::*;
pub(crate) use parse::*;
pub(crate) use codegen::*;

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
    /// `static` storage class — TU-private symbol, suppress PUBDEF.
    pub is_static: bool,
    /// `extern` storage class — symbol is defined elsewhere. Skip
    /// COMDEF + storage; register as an EXTDEF instead.
    pub is_extern: bool,
    /// `true` for `unsigned` globals (`unsigned long`, `unsigned int`).
    /// Selects unsigned codegen (e.g. SHR vs SAR, ja/jb vs jg/jl).
    pub is_unsigned: bool,
    /// `true` for `float`/`double` globals. Storage is `element_size`
    /// bytes (4 or 8); loads/stores use x87 `fld`/`fstp` and an `(int)`
    /// cast lowers to `call __ftol`.
    pub is_float: bool,
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
    /// IEEE float/double initializer — `double g = 3.14;`. The `u64`
    /// holds the f64 bits; `usize` is the byte width (8 for `double`,
    /// 4 for `float`, in which case the value is collapsed to f32).
    FloatBits(u64, usize),
}

impl GlobalInit {
    fn size_bytes(&self) -> usize {
        match self {
            GlobalInit::Byte(_) => 1,
            GlobalInit::FloatBits(_, width) => *width,
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
    /// True when the function returns `long` — return value occupies DX:AX.
    pub return_long: bool,
    /// True when the function returns `char` — callers should cbw the
    /// AL result into AX before using it as an int.
    pub return_char: bool,
    /// Byte width (4/8) of a `float`/`double` return, 0 otherwise. The value
    /// is returned via the `__fac` floating accumulator: the callee does
    /// `fstp QWORD __fac; mov ax, OFFSET __fac`, returning AX = &__fac.
    pub return_float_width: usize,
    pub params: Vec<String>,
    /// Parallel to `params`: true when the corresponding parameter is
    /// declared as `char` (signed or unsigned). Used to emit byte-compare
    /// and byte-load codegen for char params (fixtures 3121, 3130 etc).
    pub param_is_char: Vec<bool>,
    /// Parallel to `params`: true when the corresponding parameter is `long`.
    pub param_is_long: Vec<bool>,
    /// Parallel to `params`: true when param is `unsigned int` (not char, not pointer).
    pub param_is_unsigned: Vec<bool>,
    /// Parallel to `params`: byte width of a `float`/`double` param (4 or 8),
    /// 0 for non-float params. Selects D9/DD x87 width for `(int)<param>`.
    pub param_float_width: Vec<usize>,
    /// Parallel to `params`: pointee byte size for pointer params
    /// (char*→1, int*/struct*→2, long*→4, float*→width), 0 for
    /// non-pointer params. Drives pointer-difference element scaling.
    pub param_pointee_size: Vec<usize>,
    pub locals: Vec<LocalSpec>,
    /// Names parallel to `locals` — used to compute MSC's hash-table
    /// traversal order for frame slot assignment.
    pub local_names: Vec<String>,
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
    /// True for `int far *p` / `int huge *p` — storage is 4 bytes
    /// (2-byte offset at disp + 2-byte segment at disp+2). Uses
    /// `les`/`mov es:[bx]` codegen for deref.
    pub is_far_ptr: bool,
    /// For pointer locals (`int *p`, `char *p`): the byte size of the
    /// pointed-to element (1 for char*, 2 for int*). Zero for non-pointer
    /// locals. Used to compute the step for postfix `p++`/`p--`.
    pub pointee_size: usize,
    /// True for `unsigned char x` — load uses `sub ah, ah` (zero-extend)
    /// instead of `cbw` (sign-extend).
    pub is_unsigned: bool,
    /// True when the init expression was an explicit `(char)` cast from a
    /// wider type. MSC uses `b0 imm8; 88 46 disp` for these in the prologue
    /// instead of the direct `c6 46 disp imm8` form. Fixture 1039.
    pub init_via_cast: bool,
    /// True when the init expression started with a type-cast prefix such as
    /// `(int)`, `(unsigned int)`, `(char)` etc.  MSC does NOT fold the init
    /// value into later uses when a type cast is involved — the local is read
    /// from its slot at runtime. Fixture 1732.
    pub init_via_type_cast: bool,
    /// True for `float`/`double` locals — storage is `size` bytes (4 or 8),
    /// init goes through the x87 const-literal pool rather than `init`.
    pub is_float: bool,
    /// For a float/double local with a literal initializer, the f64 bits of
    /// the value, materialized in the CONST pool and loaded via `fld`.
    pub float_bits: Option<u64>,
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
    /// `Some(struct_idx)` when this field is itself a (non-pointer) struct,
    /// enabling multi-level member access (`o.inner.a`).
    pub struct_idx: Option<usize>,
}

impl LocalSpec {
    pub fn int(init: Option<i32>) -> Self {
        Self { size: 2, array_len: 1, init, struct_idx: None, is_long: false, init_is_literal: init.is_some(), is_far_ptr: false, pointee_size: 0, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None }
    }
    pub fn char_(init: Option<i32>) -> Self {
        Self { size: 1, array_len: 1, init, struct_idx: None, is_long: false, init_is_literal: init.is_some(), is_far_ptr: false, pointee_size: 0, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None }
    }
    pub fn long_(init: Option<i32>) -> Self {
        Self { size: 2, array_len: 2, init, struct_idx: None, is_long: true, init_is_literal: init.is_some(), is_far_ptr: false, pointee_size: 0, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: false, float_bits: None }
    }
    /// `float`/`double` local. `width` is 4 (float) or 8 (double); `bits` is
    /// the f64 value of a literal initializer (None for uninitialized). `init`
    /// carries the truncated int value so `(int)f` const-folds.
    pub fn float_(width: usize, bits: Option<u64>) -> Self {
        let init = bits.map(|b| f64::from_bits(b) as i32);
        Self { size: width, array_len: 1, init, struct_idx: None, is_long: false, init_is_literal: init.is_some(), is_far_ptr: false, pointee_size: 0, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: true, float_bits: bits }
    }
    /// A `float`/`double` local whose initializer is a const-foldable cast or
    /// arithmetic (`(float)i`, `double d = f`, `a + b`) rather than a direct
    /// literal. `float_bits` materializes the CONST temp, but `init` is `None`
    /// so the int-fold view does NOT replace `(int)<local>` with `mov ax,K`;
    /// instead the store keeps st(0) live (`fst`) and the cast is `call __ftol`.
    pub fn float_nonliteral(width: usize, bits: u64) -> Self {
        Self { size: width, array_len: 1, init: None, struct_idx: None, is_long: false, init_is_literal: false, is_far_ptr: false, pointee_size: 0, is_unsigned: false, init_via_cast: false, init_via_type_cast: false, is_float: true, float_bits: Some(bits) }
    }
    /// Bytes occupied in the frame, rounded up to an even count.
    /// MSC pads each local to a word boundary — scalar char gets 2
    /// bytes, char[3] gets 4 bytes, int[3] gets 6 bytes. Fixture 1134.
    /// Struct locals carry the struct's natural total_bytes
    /// (also even-padded). Far/huge pointers occupy 4 bytes (offset + segment).
    pub fn storage_bytes(&self) -> usize {
        if self.is_far_ptr { return 4; }
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
    /// Parallel-indexed flags marking globals whose element size is
    /// 1 (char). Used to pick byte-load (`a0`) + cbw over word-load.
    pub char_globals: &'a [bool],
    /// Parallel-indexed flags marking `unsigned` globals. Selects
    /// unsigned codegen (SHR vs SAR, ja/jb vs jg/jl for long compares).
    pub unsigned_globals: &'a [bool],
    /// Parallel-indexed byte width (4/8) of `float`/`double` globals,
    /// 0 otherwise. Selects x87 `fld`/`fstp` width for float-global access.
    pub float_globals: &'a [usize],
    /// Parallel-indexed flags marking locals that are `long`. Direct
    /// loads (return, assign) bypass the fold view so the slot is
    /// read at runtime even when its constant value is known.
    pub long_locals: &'a [bool],
    /// Parallel-indexed: true iff the local's init came from a pure
    /// literal expression. Char locals fold for bare reads only when
    /// this is true (fixture 1023 vs 1046).
    pub init_literals: &'a [bool],
    /// Parallel-indexed: true for `int far *p` / `int huge *p` locals.
    /// Uses les+ES-override codegen for deref; 4-byte frame slot.
    pub far_ptr_locals: &'a [bool],
    /// Parallel-indexed: true when array_len > 1, i.e. the local is an
    /// array. Used to distinguish array decay (`p = a`) from value
    /// copy in far-pointer assignment codegen.
    pub array_locals: &'a [bool],
    /// Parallel-indexed: true for `unsigned char x` locals — load uses
    /// `sub ah, ah` (zero-extend) instead of `cbw` (sign-extend).
    pub unsigned_locals: &'a [bool],
    /// Parallel-indexed byte width (4/8) of `float`/`double` locals, 0
    /// otherwise. A non-literal float local consumed by `(int)<local>` is
    /// stored with `fst` (st(0) kept live) and the cast is bare `call __ftol`.
    pub float_locals: &'a [usize],
    /// Parallel to function params: true when that param is `char`
    /// typed. Used to emit byte-compare / byte-load codegen.
    pub char_params: &'a [bool],
    /// Parallel to function params: true when that param is `long`.
    pub long_params: &'a [bool],
    /// Parallel to function params: true when that param is `unsigned int`
    /// (not char, not signed). Used to emit `shr ax,1` for /2 vs `cwd+sar`.
    pub unsigned_params: &'a [bool],
    /// Parallel to function params: byte width of a `float`/`double` param
    /// (4 or 8), 0 otherwise. Selects the x87 width (D9/DD) when an `(int)`
    /// cast of the param lowers to `fld [bp+disp]; call __ftol`.
    pub param_float_widths: &'a [usize],
    /// Parallel to function params: pointee byte size for pointer params
    /// (0 for non-pointers). Drives pointer-difference element scaling
    /// (`p - q` → byte sub then `sar` to convert bytes → elements).
    pub param_pointee_sizes: &'a [usize],
    /// Map of function names that return `char`. The caller inserts
    /// `cbw` after the call to widen AL to AX (fixture 1006).
    pub char_returners: &'a std::collections::HashSet<String>,
    /// Map of function symbol names to their param_is_long arrays.
    /// Used by emit_call_inner to push long args as 4-byte pairs.
    pub long_param_funcs: &'a std::collections::HashMap<String, Vec<bool>>,
    /// Map of `float`/`double`-returning function symbol names to their return
    /// width (4/8). The result comes back as AX = &__fac; assigning the call to
    /// a float local triggers the `movsw`-copy receive sequence.
    pub float_returners: &'a std::collections::HashMap<String, usize>,
    /// Stack of in-progress loops. Each entry's `breaks` and
    /// `continues` collect placeholder-jump offsets within the loop
    /// body's emit buffer, to be patched at loop end. RefCell so
    /// emit_stmt (which only takes `&Locals`) can mutate.
    pub loop_stack: &'a std::cell::RefCell<Vec<LoopCtx>>,
    /// FpuStack state: `Some(local_idx)` when that float local's value is
    /// currently live on x87 `st(0)` (its init was stored with `fst`, keeping
    /// the stack top). A coupled `return (int)<local>` consumes it with a bare
    /// `call __ftol`; otherwise the value must be reloaded with `fld` first.
    pub fpu_live: &'a std::cell::Cell<Option<usize>>,
    /// Byte width (4/8) of the enclosing function's `float`/`double` return,
    /// 0 otherwise. A `return <float>` emits the `__fac` accumulator sequence.
    pub return_float_width: usize,
    /// BP-relative displacement of the hidden 8-byte temp used to receive a
    /// `return (int)<float-returning call>` result, 0 when unused.
    pub float_call_temp_disp: i16,
    /// Set after a body-level float store (`a[k] = K.Ff`) leaves a pending x87
    /// store; the `90 9B` fwait is flushed before the next non-FP statement.
    pub fpu_pending_fwait: &'a std::cell::Cell<bool>,
}

#[derive(Default, Debug)]
pub struct LoopCtx {
    pub breaks: Vec<usize>,
    pub continues: Vec<usize>,
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
    pub fn is_char_global(&self, idx: usize) -> bool {
        self.char_globals.get(idx).copied().unwrap_or(false)
    }
    pub fn is_unsigned_global(&self, idx: usize) -> bool {
        self.unsigned_globals.get(idx).copied().unwrap_or(false)
    }
    /// 4 or 8 for a `float`/`double` global, else 0.
    pub fn float_global_width(&self, idx: usize) -> usize {
        self.float_globals.get(idx).copied().unwrap_or(0)
    }
    pub fn is_float_global(&self, idx: usize) -> bool {
        self.float_global_width(idx) != 0
    }
    pub fn is_long_local(&self, idx: usize) -> bool {
        self.long_locals.get(idx).copied().unwrap_or(false)
    }
    pub fn init_is_literal(&self, idx: usize) -> bool {
        self.init_literals.get(idx).copied().unwrap_or(false)
    }
    pub fn is_far_ptr_local(&self, idx: usize) -> bool {
        self.far_ptr_locals.get(idx).copied().unwrap_or(false)
    }
    pub fn is_array_local(&self, idx: usize) -> bool {
        self.array_locals.get(idx).copied().unwrap_or(false)
    }
    pub fn is_unsigned_local(&self, idx: usize) -> bool {
        self.unsigned_locals.get(idx).copied().unwrap_or(false)
    }
    /// 4 or 8 for a `float`/`double` local, else 0.
    pub fn float_local_width(&self, idx: usize) -> usize {
        self.float_locals.get(idx).copied().unwrap_or(0)
    }
    pub fn is_float_local(&self, idx: usize) -> bool {
        self.float_local_width(idx) != 0
    }
    pub fn is_char_param(&self, idx: usize) -> bool {
        self.char_params.get(idx).copied().unwrap_or(false)
    }
    pub fn is_long_param(&self, idx: usize) -> bool {
        self.long_params.get(idx).copied().unwrap_or(false)
    }
    pub fn is_unsigned_param(&self, idx: usize) -> bool {
        self.unsigned_params.get(idx).copied().unwrap_or(false)
    }
    /// 4 or 8 for a `float`/`double` param, else 0.
    pub fn float_param_width(&self, idx: usize) -> usize {
        self.param_float_widths.get(idx).copied().unwrap_or(0)
    }
    pub fn is_float_param(&self, idx: usize) -> bool {
        self.float_param_width(idx) != 0
    }
    /// Pointee byte size of a pointer param, else 0.
    pub fn param_pointee_size(&self, idx: usize) -> usize {
        self.param_pointee_sizes.get(idx).copied().unwrap_or(0)
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

#[derive(Debug, Clone)]
pub enum AssignTarget {
    Local(usize),
    /// Assigning to a function parameter — addressed via `[bp+pdisp]`
    /// rather than `[bp-disp]`. C semantics: only mutates the local
    /// copy. Fixture 1224.
    Param(usize),
    /// `*<ptr-param> = <expr>;` — store through a pointer parameter.
    /// Codegen: `mov bx, [bp+pdisp]; mov [bx], imm/ax`.
    DerefParam(usize),
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
    /// `<struct-ptr-global>-><field> = <expr>;` — store via a
    /// struct-pointer global. Loads `[global]` into BX, then
    /// `c7 47 off imm16` / `c6 47 off imm8`.
    DerefGlobalField { ptr_global: usize, byte_off: u16, size: u8 },
    /// `<local-int-array>[K] = <expr>;` — write a word at a constant
    /// index. `byte_off` is `K * 2`. Codegen uses BP-rel store at
    /// `locals.disp(local) + byte_off`.
    IndexedLocal { local: usize, byte_off: u16 },
    /// `<local-char-array>[K] = <byte>;` — write a byte at a constant
    /// index. `byte_off` is `K`. Codegen uses `c6 46 disp imm8`.
    IndexedLocalByte { local: usize, byte_off: u16 },
    /// `<local-int-array>[<expr>] = <expr>;` — write a word at a
    /// runtime index. Codegen: `mov si, [idx_disp]; shl si, 1;
    /// <eval rhs→AX>; mov [bp+si+base_disp], ax`. Fixtures 1468.
    IndexedLocalVar { local: usize, index: Box<Expr> },
    /// `<local-char-array>[<expr>] = <byte>;` — write a byte at a
    /// runtime index. Codegen: `mov si, [idx_disp]; <eval rhs→AL>;
    /// mov [bp+si+base_disp], al`. Fixture 1219.
    IndexedLocalByteVar { local: usize, index: Box<Expr> },
    /// `**<global-ptr-to-ptr> = <expr>;` — double-deref store. Codegen:
    /// `mov bx, [global]; mov bx, [bx]; mov [bx], ax`.
    DoubleDerefGlobal(usize),
    /// `**<local-ptr-to-ptr> = <expr>;` — double-deref store through a local.
    /// Usually resolved to a direct lvalue by the alias pass (pp -> p -> x);
    /// the fallback codegen is `mov bx, [bp-pp]; mov bx, [bx]; mov [bx], ax`.
    DoubleDerefLocal(usize),
    /// `*<ptr-local>++ = <expr>;` — store through the OLD pointer value,
    /// then advance the pointer by `step`. Codegen: `mov bx, [bp-p];
    /// <mutate p>; mov [bx], ax/imm`.
    DerefPostMutateLocal { local_idx: usize, step: i32 },
    /// `a[i][j] = <expr>;` store on a 2-D array with a runtime index. Mirrors
    /// `Expr::Index2D`; const-prop folds to a flat IndexedGlobal/IndexedLocal when
    /// both indices are known, else codegen does the `si`/`bx` addressing.
    Index2D { is_global: bool, base: usize, row: Box<Expr>, col: Box<Expr>, cols: usize, elem: usize },
    /// `<ptr-local>[K] = <expr>;` with constant K≠0 — store through a
    /// pointer local at a byte offset (`byte_off` = K * pointee size).
    /// When the pointer aliases a base array the const-prop pass rewrites
    /// this to an `IndexedLocal`/`IndexedGlobal` direct store; otherwise the
    /// fallback is `mov bx, [bp-p]; mov [bx+byte_off], ax/imm`.
    DerefLocalOffset { local: usize, byte_off: u16, is_byte: bool },
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
    /// A floating-point literal: the f64 value's raw bits and whether the
    /// source was `double` (true) or `float`/`3.0f` (false). Materialized in
    /// the CONST float-literal pool; `fold` truncates it for `(int)` casts.
    FloatLit(u64, bool),
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
    /// `a[i][j]` read on a 2-D array with a runtime index. `base` is the
    /// global (or local, when `is_global` is false) index, `cols` the inner
    /// dimension, `elem` the element byte size. Const-prop folds this to a flat
    /// 1-D access when both indices are known; otherwise codegen materializes
    /// `si = row*cols*elem`, `bx = col*elem` and reads `[base + bx + si]`.
    Index2D { is_global: bool, base: usize, row: Box<Expr>, col: Box<Expr>, cols: usize, elem: usize },
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
    /// `<struct-ptr-global>-><field>` — deref through a struct-ptr
    /// global. `mov bx, [global]; mov ax, [bx+off]`.
    DerefGlobalField { ptr_global: usize, byte_off: u16, size: u8 },
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
    /// `(<stmt>, <stmt>, ..., <expr>)` — comma operator. The
    /// statements run for their side effects; the final expr's value
    /// is the yielded value. Synthesized at parse-time when we see
    /// `(<assign>, ...)`. Fixture 1057.
    Seq {
        sides: Vec<Stmt>,
        value: Box<Expr>,
    },
    /// `*<ptr>` — byte-sized pointer dereference (`char *`). Lowers
    /// to `mov bx, <ptr>; mov al, [bx]; cbw`. Fixture 4111.
    DerefByte { ptr: Box<Expr> },
    /// `*<ptr>` — word-sized pointer dereference (`int *`). Lowers
    /// to `mov bx, <ptr>; mov ax, [bx]`. Fixture 4125.
    DerefWord { ptr: Box<Expr> },
    /// `<local>++` or `<local>--` — evaluates to the OLD value of the
    /// local and then mutates the local by `step` (±1 for scalars,
    /// ±pointee_size for pointer locals). Used in conditions, call
    /// args, and deref-postmutate expressions. Step encodes both
    /// direction (sign) and magnitude (pointer stride).
    PostMutateLocal { local_idx: usize, step: i32 },
    /// `<global>++` or `<global>--` — same semantics as PostMutateLocal
    /// but targeting a file-scope variable. Requires a GlobalAddr fixup
    /// for both the load and the mutate instruction.
    PostMutateGlobal { global_idx: usize, step: i32 },
    /// `++<local>` or `--<local>` — pre-increment/decrement. Mutates the
    /// local first (inc/dec/add), then evaluates to the NEW value.
    PreMutateLocal { local_idx: usize, step: i32 },
    /// `++<global>` or `--<global>` — pre-increment/decrement of a
    /// file-scope variable. Mutates first, then evaluates to the NEW value.
    PreMutateGlobal { global_idx: usize, step: i32 },
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
            // `(int)<float-const>` truncates toward zero.
            Expr::FloatLit(bits, _) => Some(f64::from_bits(*bits) as i32),
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
            Expr::Index2D { .. } => None,
            Expr::LocalIndex { .. } | Expr::LocalIndexByte { .. } => None,
            Expr::ParamIndex { .. } => None,
            Expr::LocalField { .. } | Expr::DerefLocalField { .. } | Expr::GlobalField { .. } => None,
            Expr::DerefParamField { .. } | Expr::DerefGlobalField { .. } => None,
            Expr::DerefByte { .. } | Expr::DerefWord { .. } => None,
            Expr::AddrOfGlobal(_) | Expr::AddrOfLocal(_) => None,
            Expr::PostMutateLocal { .. } | Expr::PostMutateGlobal { .. }
            | Expr::PreMutateLocal { .. } | Expr::PreMutateGlobal { .. } => None,
            // Comma expression fold: if all the side stmts have no
            // observable side effect (just discard a value), fold to
            // the tail's value. Otherwise refuse to fold (the assigns
            // would still need to run at runtime).
            Expr::Seq { sides, value } => {
                let all_pure = sides.iter().all(|s| matches!(s,
                    Stmt::ExprStmt(e) if e.fold(locals).is_some()
                        || matches!(e, Expr::Local(_) | Expr::Param(_) | Expr::Global(_) | Expr::StrLit(_) | Expr::AddrOfGlobal(_) | Expr::AddrOfLocal(_))
                ));
                if all_pure {
                    value.fold(locals)
                } else {
                    None
                }
            }
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
    /// Floating-point literal: f64 bits + whether it was a `double`.
    Float(u64, bool),
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
    /// Parallel to `param_names`: true when the param is `char` typed.
    param_is_char: Vec<bool>,
    /// Parallel to `param_names`: true when the param is `long`.
    param_is_long: Vec<bool>,
    /// Parallel to `param_names`: true when the param is `unsigned int`.
    param_is_unsigned: Vec<bool>,
    /// Parallel to `param_names`: pointee byte size for pointer params (char*→1,
    /// int*→2, …), 0 for non-pointers. Drives `*(p + i)` byte/word selection.
    param_pointee_sizes: Vec<usize>,
    /// File-scope global names in source order; the index doubles
    /// as the `Expr::Global(idx)` value.
    global_names: Vec<String>,
    /// Same source order, used to materialize the `Unit::globals`.
    globals: Vec<Global>,
    /// Multidimensional array shapes, keyed by global / local index.
    /// `[2,3]` for `int a[2][3]`. Absent for scalars and 1-D arrays.
    /// Lets `a[i][j]` with constant indices fold to a flat element offset
    /// (row-major: `i*3 + j`) reusing the 1-D Index codegen.
    global_dims: std::collections::HashMap<usize, Vec<usize>>,
    local_dims: std::collections::HashMap<usize, Vec<usize>>,
    /// Named struct definitions collected at parse time, by source
    /// order. The position in the Vec is the `struct_idx` referenced
    /// by `LocalSpec::struct_idx` and `Global::struct_idx`.
    structs: Vec<StructDef>,
    /// Strings interned across the whole translation unit. New
    /// string literals append; duplicates currently get distinct
    /// entries (no dedup yet — no fixture exercises a repeated
    /// literal).
    strings: Vec<Vec<u8>>,
    /// Compile-time integer constants from enum declarations.
    /// Looked up at every Ident parse so `N` from `enum { N = 4 }`
    /// substitutes as `IntLit(4)`.
    enum_consts: std::collections::HashMap<String, i32>,
    /// `typedef <type> <alias>;` aliases. Each alias maps to one of
    /// the recognized primitive type names ("int", "char", "long").
    typedefs: std::collections::HashMap<String, &'static str>,
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

#[derive(Debug, Clone)]
struct Fixup {
    /// Offset of the placeholder bytes within `FunctionEmit::bytes`.
    /// For `e8 disp16` calls this is the offset of the `e8` opcode
    /// (disp bytes at +1, +2); for `b8 imm16` string loads this is
    /// the offset of the `b8` opcode (imm bytes at +1, +2).
    body_offset: usize,
    kind: FixupKind,
}

#[derive(Debug, Clone)]
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
    /// Load of an x87 float/double literal from the CONST pool
    /// (`fld <dword|qword> [$T]`). Same CONST/DGROUP segment-relative FIXUP
    /// shape as StrLoad (`c4 off 9c`); resolved via the (bits,width) pool.
    FloatLoad { bits: u64, width: usize },
    /// FP-emulator marker fixup on an x87 instruction's leading byte: a
    /// seg-relative external reference (`c4 off 56 <idx>`) to FIDRQQ/FIWRQQ so
    /// the linker can rewrite the site for the emulator. The fixup offset is
    /// the instruction byte itself (no +1).
    FloatMarker { target: &'static str },
    /// Reference to an initialized file-scope global at a known
    /// offset within `_DATA`. The FIXUP uses DGROUP-as-frame and
    /// _DATA-as-target via the pre-emitted threads (`c4 off 9d`).
    /// Fixtures 4104, 4106.
    GlobalAddr { global_idx: usize },
    /// Reference to a runtime *data* extern by name (e.g. `__fac`, the
    /// floating accumulator). Seg-relative external FIXUP `c4 off 56 <idx>`
    /// (same shape as a COMDEF GlobalAddr). The placeholder stays 0.
    ExtData { target: &'static str },
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
    /// Like WithSlide but also saves/restores SI for runtime local
    /// array indexing. Prologue adds `push si` after chkstk.
    /// Epilogue: `pop si; mov sp, bp; pop bp; ret` (fixtures 1219, 1468).
    WithSlideSi,
    /// Like BpOnly but saves/restores SI — a no-locals function that uses a
    /// runtime 2-D index (SI). Prologue adds `push si` after chkstk; SP doesn't
    /// slide so the epilogue is `pop si; pop bp; ret` (fixtures 3327, 3544).
    BpOnlySi,
    /// Like WithSlide but saves/restores DI then SI — needed for the `movsw`
    /// copy that receives a float/double return into a local. Prologue adds
    /// `push di; push si` after chkstk; epilogue `pop si; pop di; mov sp,bp;
    /// pop bp; ret` (fixtures 1684, 2144, 4001).
    WithSlideDiSi,
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
            Frame::BpOnlySi => &[0x5E, 0x5D, 0xC3],
            Frame::WithSlide => &[0x8B, 0xE5, 0x5D, 0xC3],
            Frame::WithSlideSi => &[0x5E, 0x8B, 0xE5, 0x5D, 0xC3],
            Frame::WithSlideDiSi => &[0x5E, 0x5F, 0x8B, 0xE5, 0x5D, 0xC3],
        }
    }
    fn is_with_slide(self) -> bool {
        matches!(self, Frame::WithSlide | Frame::WithSlideSi | Frame::WithSlideDiSi)
    }
}

/// MSC's fixed floating-point-emulator EXTDEF block, emitted (before
/// `__acrtused`) whenever the unit uses FP. FIDRQQ/FIWRQQ are the markers the
/// per-FP-instruction fixups target.
fn fp_extern_block(uses_float: bool) -> &'static [(&'static str, u8)] {
    if uses_float {
        // __fltused uses type-index 0; the FxxRQQ emulator markers use 1.
        &[
            ("__fltused", 0x00),
            ("FJSRQQ", 0x01), ("FISRQQ", 0x01), ("FIERQQ", 0x01),
            ("FIDRQQ", 0x01), ("FIWRQQ", 0x01),
        ][..]
    } else {
        &[]
    }
}

/// Walk an expression, collecting `(bits, width)` for every float/double
/// literal passed as a call argument (width 8 for `double`, 4 for `float`).
/// These materialize as CONST `$T` temps, loaded with `fld` at the call site.
fn collect_call_float_args_expr(e: &Expr, out: &mut Vec<(u64, usize)>) {
    match e {
        Expr::Call { args, .. } => {
            for a in args {
                if let Expr::FloatLit(bits, is_double) = a {
                    out.push((*bits, if *is_double { 8 } else { 4 }));
                }
                collect_call_float_args_expr(a, out);
            }
        }
        Expr::BinOp { left, right, .. } => {
            collect_call_float_args_expr(left, out);
            collect_call_float_args_expr(right, out);
        }
        Expr::Ternary { cond, then_arm, else_arm } => {
            collect_call_float_args_expr(cond, out);
            collect_call_float_args_expr(then_arm, out);
            collect_call_float_args_expr(else_arm, out);
        }
        Expr::Seq { sides, value } => {
            for s in sides { collect_call_float_args_stmt(s, out); }
            collect_call_float_args_expr(value, out);
        }
        _ => {}
    }
}

fn collect_call_float_args_stmt(s: &Stmt, out: &mut Vec<(u64, usize)>) {
    match s {
        Stmt::Return(e) | Stmt::ExprStmt(e) => collect_call_float_args_expr(e, out),
        Stmt::Assign { value, .. } => collect_call_float_args_expr(value, out),
        Stmt::If { then_branch, else_branch, .. } => {
            collect_call_float_args_stmt(then_branch, out);
            if let Some(eb) = else_branch { collect_call_float_args_stmt(eb, out); }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
            collect_call_float_args_stmt(body, out);
        }
        Stmt::For { init, step, body, .. } => {
            collect_call_float_args_stmt(init, out);
            collect_call_float_args_stmt(step, out);
            collect_call_float_args_stmt(body, out);
        }
        Stmt::Block(ss) => for s in ss { collect_call_float_args_stmt(s, out); },
        Stmt::Switch { cases, .. } => {
            for c in cases { for s in &c.body { collect_call_float_args_stmt(s, out); } }
        }
        _ => {}
    }
}

/// Collect `(bits, width)` for every float/double literal stored to a global
/// (`g = 3.14;`). These intern into the CONST pool as `$T` temps loaded by the
/// float-global store sequence (`fld $T; fstp [g]`). Kept separate from the
/// call-arg walker so it does not affect the WithSlide frame decision.
fn collect_global_store_floats_stmt(s: &Stmt, out: &mut Vec<(u64, usize)>) {
    match s {
        Stmt::Assign { target: AssignTarget::Global(_), value: Expr::FloatLit(bits, is_double) } => {
            out.push((*bits, if *is_double { 8 } else { 4 }));
        }
        Stmt::If { then_branch, else_branch, .. } => {
            collect_global_store_floats_stmt(then_branch, out);
            if let Some(eb) = else_branch { collect_global_store_floats_stmt(eb, out); }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
            collect_global_store_floats_stmt(body, out);
        }
        Stmt::For { init, step, body, .. } => {
            collect_global_store_floats_stmt(init, out);
            collect_global_store_floats_stmt(step, out);
            collect_global_store_floats_stmt(body, out);
        }
        Stmt::Block(ss) => for s in ss { collect_global_store_floats_stmt(s, out); },
        Stmt::Switch { cases, .. } => {
            for c in cases { for s in &c.body { collect_global_store_floats_stmt(s, out); } }
        }
        _ => {}
    }
}

/// True when the function passes a float/double literal as a call argument.
/// Such a call needs the `fld; sub sp; mov bx,sp; fstp [bx]; fwait` push
/// sequence, which slides SP — forcing a WithSlide frame and a result temp.
pub(crate) fn func_has_float_arg_call(func: &Function) -> bool {
    let mut v = Vec::new();
    for s in &func.body { collect_call_float_args_stmt(s, &mut v); }
    !v.is_empty()
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
    let char_globals: Vec<bool> = unit.globals.iter().map(|g| !g.is_pointer && g.element_size == 1 && g.array_len == 1).collect();
    let global_elem_sizes: Vec<usize> = unit.globals.iter().map(|g| g.element_size).collect();
    let unsigned_globals: Vec<bool> = unit.globals.iter().map(|g| g.is_unsigned).collect();
    let float_globals: Vec<usize> = unit.globals.iter()
        .map(|g| if g.is_float { g.element_size } else { 0 }).collect();
    let char_returners: std::collections::HashSet<String> = unit.functions.iter()
        .filter(|f| f.return_char)
        .map(|f| symbol_name(&f.name))
        .collect();
    let long_param_funcs: std::collections::HashMap<String, Vec<bool>> = unit.functions.iter()
        .filter(|f| f.param_is_long.iter().any(|&b| b))
        .map(|f| (symbol_name(&f.name), f.param_is_long.clone()))
        .collect();
    let float_returners: std::collections::HashMap<String, usize> = unit.functions.iter()
        .filter(|f| f.return_float_width != 0)
        .map(|f| (symbol_name(&f.name), f.return_float_width))
        .collect();
    let function_emits: Vec<FunctionEmit> = unit
        .functions
        .iter()
        .map(|f| emit_function(f, &long_globals, &char_globals, &unsigned_globals, &float_globals, &global_elem_sizes, &char_returners, &float_returners, &long_param_funcs))
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
    // Float-literal pool: distinct (bits, width) from all float/double
    // locals, placed in CONST after the strings (word-aligned, `width` bytes
    // each). MSC materializes these as `$T` DD/DQ temps.
    let mut float_pool: Vec<(u64, usize)> = Vec::new();
    for f in &unit.functions {
        for l in &f.locals {
            if l.is_float
                && let Some(bits) = l.float_bits
                && !float_pool.contains(&(bits, l.size))
            {
                float_pool.push((bits, l.size));
            }
        }
    }
    // Float/double literals passed as call arguments also intern into the
    // CONST pool (the caller does `fld $T; ...; fstp [bx]` to push them).
    for f in &unit.functions {
        let mut args = Vec::new();
        for s in &f.body { collect_call_float_args_stmt(s, &mut args); }
        for (bits, width) in args {
            if !float_pool.contains(&(bits, width)) {
                float_pool.push((bits, width));
            }
        }
    }
    // …as do float/double literals stored to a global (`g = 3.14;`).
    for f in &unit.functions {
        let mut stores = Vec::new();
        for s in &f.body { collect_global_store_floats_stmt(s, &mut stores); }
        for (bits, width) in stores {
            if !float_pool.contains(&(bits, width)) {
                float_pool.push((bits, width));
            }
        }
    }
    // …and the const-folded value of a `float`/`double` `return` (parse folds
    // it to a FloatLit), materialized as a CONST temp for the __fac sequence.
    for f in &unit.functions {
        if f.return_float_width == 0 { continue; }
        for s in &f.body {
            if let Stmt::Return(Expr::FloatLit(bits, is_double)) = s {
                let w = if *is_double { 8 } else { 4 };
                if !float_pool.contains(&(*bits, w)) {
                    float_pool.push((*bits, w));
                }
            }
        }
    }
    // …and float/double array element stores (`a[k] = 1.0f`) — width from the
    // literal (a float array gets float literals, a double array doubles).
    for f in &unit.functions {
        for s in &f.body {
            if let Stmt::Assign { target: AssignTarget::IndexedLocal { local, .. }, value: Expr::FloatLit(bits, is_double) } = s
                && f.locals.get(*local).map(|l| l.is_float).unwrap_or(false)
            {
                let w = if *is_double { 8 } else { 4 };
                if !float_pool.contains(&(*bits, w)) {
                    float_pool.push((*bits, w));
                }
            }
        }
    }
    // …and the operand of `return (int)(<float-global-array>[K] op lit)` — a
    // width-8 (double) temp.
    for f in &unit.functions {
        for s in &f.body {
            if let Stmt::Return(Expr::BinOp { left, right, .. }) = s
                && let Expr::Index { array, .. } = left.as_ref()
                && unit.globals.get(*array).map(|g| g.is_float).unwrap_or(false)
                && let Expr::FloatLit(bits, _) = right.as_ref()
                && !float_pool.contains(&(*bits, 8))
            {
                float_pool.push((*bits, 8));
            }
        }
    }
    // …and the literal operand of a coupled FP op (`return (int)(f + 1.5f)` or a
    // compound assign `d += 5.5`) — always a width-8 (double) temp.
    for f in &unit.functions {
        for s in &f.body {
            let operand = match s {
                Stmt::Return(Expr::BinOp { left, right, .. })
                | Stmt::Assign { value: Expr::BinOp { left, right, .. }, .. } => {
                    match (left.as_ref(), right.as_ref()) {
                        (Expr::Local(li), Expr::FloatLit(bits, _))
                            if f.locals.get(*li).map(|l| l.is_float).unwrap_or(false) => Some(*bits),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(bits) = operand
                && !float_pool.contains(&(bits, 8))
            {
                float_pool.push((bits, 8));
            }
        }
    }
    if !float_pool.is_empty() && const_cursor % 2 != 0 {
        const_cursor += 1;
    }
    let mut float_offsets: Vec<usize> = Vec::with_capacity(float_pool.len());
    for (_, width) in &float_pool {
        float_offsets.push(const_cursor);
        const_cursor += width;
    }
    // A function with a float/double parameter emits x87 FIDRQQ markers even
    // when the unit has no CONST float temp, so the FxxRQQ EXTDEF block must
    // still be present.
    let any_float_param = unit.functions.iter()
        .any(|f| f.param_float_width.iter().any(|w| *w != 0));
    let any_float_global = unit.globals.iter().any(|g| g.is_float);
    // A float-returning function emits x87 (fild/fld/fstp __fac); its callers do
    // the same (`fld temp; __ftol`). Either way the FxxRQQ block must be present.
    let any_float_return = unit.functions.iter().any(|f| f.return_float_width != 0);
    let uses_float = !float_pool.is_empty() || any_float_param || any_float_global
        || any_float_return;
    let float_offset_of = |bits: u64, width: usize| -> usize {
        let idx = float_pool.iter().position(|e| *e == (bits, width)).expect("float in pool");
        float_offsets[idx]
    };

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

    // Runtime-helper externs referenced via ExtCall (e.g. __aNNalshl for long
    // shift compound-assign). These sit right after __chkstk in the EXTDEF
    // table, in first-reference order. __chkstk itself is emitted explicitly.
    // Runtime data externs (e.g. __fac) referenced via ExtData interleave here
    // too, in first-reference order across functions.
    let mut helper_extern_order: Vec<String> = Vec::new();
    let mut seen_helpers: std::collections::HashSet<String> = std::collections::HashSet::new();
    for fe in &function_emits {
        for fx in &fe.fixups {
            let target = match &fx.kind {
                FixupKind::ExtCall { target } if target != "__chkstk" => target.clone(),
                FixupKind::ExtData { target } => (*target).to_owned(),
                _ => continue,
            };
            if seen_helpers.insert(target.clone()) {
                helper_extern_order.push(target);
            }
        }
    }
    // The float runtime externs `__ftol`/`__fac` move AFTER the function names
    // (trailing) instead of the helper slot when a single function references
    // >= 3 distinct float CONST temps. Empirical: 1671/2138/2141/2142/2136/
    // 1673/2148/3997/4002 (one function, 3 temps) trail; <=2 temps, or temps
    // split across functions (1684, 2146), keep the helper slot. Only the
    // no-COMDEF/no-user-extern layout (a single `main`) is affected.
    let float_helpers_trailing = function_emits.iter().any(|fe| {
        let mut temps = std::collections::HashSet::new();
        for fx in &fe.fixups {
            if let FixupKind::FloatLoad { bits, width } = &fx.kind {
                temps.insert((*bits, *width));
            }
        }
        temps.len() >= 3
    });
    let is_float_helper = |h: &str| h == "__ftol" || h == "__fac";

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
    // Tentative globals (no init, not extern) → COMDEF. Externs are
    // handled separately as EXTDEFs (fixture 163).
    let comdef_globals: Vec<usize> = unit
        .globals
        .iter()
        .enumerate()
        .filter_map(|(i, g)| if g.init.is_none() && !g.is_extern { Some(i) } else { None })
        .collect();
    let extern_globals: Vec<usize> = unit
        .globals
        .iter()
        .enumerate()
        .filter_map(|(i, g)| if g.is_extern { Some(i) } else { None })
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
    // Helper: emit COMDEF record for tentative globals.
    let emit_comdef = |b: &mut ObjBuilder,
                       idx_map: &mut std::collections::HashMap<String, u8>,
                       start: &mut u8| {
        let mut payload = Vec::new();
        for &gi in &comdef_globals {
            let g = &unit.globals[gi];
            let sym = symbol_name(&g.name);
            let byte_len = g.storage_bytes();
            payload.push(u8::try_from(sym.len()).expect("COMDEF name fits"));
            payload.extend_from_slice(sym.as_bytes());
            payload.push(0x00); // type index
            payload.push(0x62); // NEAR data
            if byte_len <= 0x80 {
                payload.push(byte_len as u8);
            } else {
                payload.push(0x81);
                payload.extend_from_slice(&u16::try_from(byte_len)
                    .expect("COMDEF u16 length fits")
                    .to_le_bytes());
            }
            idx_map.insert(sym, *start);
            *start += 1;
        }
        b.write_record(0xB0, &payload);
    };
    if comdef_globals.is_empty() {
        if user_extern_order.is_empty() {
            // No splits — single combined EXTDEF.
            // Extern globals (from `extern int g;`) go between __chkstk
            // and defined-function names. Fixtures 163, 1959, 2157, 4041.
            let mut entries: Vec<(String, u8)> = Vec::new();
            // FP-emulator marker block precedes __acrtused when the unit uses
            // floating point (matches MSC's EXTDEF layout). FIDRQQ/FIWRQQ are
            // referenced by per-instruction marker fixups.
            for (m, ty) in fp_extern_block(uses_float) {
                entries.push(((*m).to_owned(), *ty));
            }
            entries.push(("__acrtused".to_owned(), 0x01));
            entries.push(("__chkstk".to_owned(), 0x00));
            for h in &helper_extern_order {
                if float_helpers_trailing && is_float_helper(h) { continue; }
                entries.push((h.clone(), 0x00));
            }
            for &gi in &extern_globals {
                entries.push((symbol_name(&unit.globals[gi].name), 0x00));
            }
            for f in &unit.functions {
                entries.push((symbol_name(&f.name), 0x00));
            }
            // The float runtime externs (`__ftol`/`__fac`) trail the function
            // names when a function uses >=3 float CONST temps, in their
            // first-reference order (see `float_helpers_trailing`).
            if float_helpers_trailing {
                for h in &helper_extern_order {
                    if is_float_helper(h) {
                        entries.push((h.clone(), 0x00));
                    }
                }
            }
            emit_group(&mut b, &entries, &mut extdef_idx_of, &mut next_idx);
        } else {
            // Has implicit user-function externs, no COMDEFs.
            // Layout: __acrtused, [user-fn-externs], [fns], __chkstk.
            // Extern globals also go after __chkstk if any (fixture 4024).
            let mut entries: Vec<(String, u8)> = Vec::new();
            for (m, ty) in fp_extern_block(uses_float) {
                entries.push(((*m).to_owned(), *ty));
            }
            entries.push(("__acrtused".to_owned(), 0x01));
            for name in &user_extern_order {
                entries.push((name.clone(), 0x00));
            }
            for f in &unit.functions {
                entries.push((symbol_name(&f.name), 0x00));
            }
            entries.push(("__chkstk".to_owned(), 0x00));
            for h in &helper_extern_order {
                entries.push((h.clone(), 0x00));
            }
            emit_group(&mut b, &entries, &mut extdef_idx_of, &mut next_idx);
            // Add any extern globals to extdef_idx_of so FIXUP generation
            // can reference them (even if order isn't perfect yet).
            for &gi in &extern_globals {
                let sym = symbol_name(&unit.globals[gi].name);
                if !extdef_idx_of.contains_key(&sym) {
                    extdef_idx_of.insert(sym, next_idx);
                    next_idx += 1;
                }
            }
        }
    } else {
        // Has COMDEFs — always use split layout regardless of user-fn-externs.
        // Fixtures 482, 3590, 3602, 424.
        // EXTDEF1: __acrtused, __chkstk
        let mut pre: Vec<(String, u8)> = Vec::new();
        for (m, ty) in fp_extern_block(uses_float) {
            pre.push(((*m).to_owned(), *ty));
        }
        pre.push(("__acrtused".to_owned(), 0x01));
        pre.push(("__chkstk".to_owned(), 0x00));
        for h in &helper_extern_order {
            pre.push((h.clone(), 0x00));
        }
        emit_group(&mut b, &pre, &mut extdef_idx_of, &mut next_idx);
        // COMDEF: tentative globals
        emit_comdef(&mut b, &mut extdef_idx_of, &mut next_idx);
        // EXTDEF2: user-fn-externs + defined functions
        let mut post: Vec<(String, u8)> = Vec::new();
        for name in &user_extern_order {
            post.push((name.clone(), 0x00));
        }
        for f in &unit.functions {
            post.push((symbol_name(&f.name), 0x00));
        }
        emit_group(&mut b, &post, &mut extdef_idx_of, &mut next_idx);
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
                // `static` globals are TU-private — skip PUBDEF.
                if unit.globals[*i].is_static { continue; }
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
                FixupKind::FloatLoad { bits, width } => {
                    let off = u16::try_from(float_offset_of(*bits, *width))
                        .expect("float CONST offset fits");
                    fe.bytes[fx.body_offset + 1] = (off & 0xFF) as u8;
                    fe.bytes[fx.body_offset + 2] = ((off >> 8) & 0xFF) as u8;
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset + 1,
                        kind: FixupKind::FloatLoad { bits: *bits, width: *width },
                    });
                }
                FixupKind::FloatMarker { target } => {
                    // The fixup lands on the instruction byte itself (no +1).
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset,
                        kind: FixupKind::FloatMarker { target },
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
                FixupKind::ExtData { target } => {
                    // Data extern (e.g. __fac); placeholder stays 0, the linker
                    // substitutes the address via the EXTDEF FIXUP.
                    ledata_fixups.push(ResolvedFixup {
                        ledata_offset: caller_off + fx.body_offset + 1,
                        kind: FixupKind::ExtData { target },
                    });
                }
            }
        }
    }

    // For each CONST float temp, the index of the first function that loads
    // it. MSC interleaves segments in compilation order: it flushes a temp's
    // CONST LEDATA just before the _TEXT of the function that introduces it.
    // Temps introduced by function 0 go in the pre-_TEXT CONST block (below);
    // temps introduced by a later function split _TEXT into separate runs.
    let mut float_intro_fn: Vec<usize> = vec![0usize; float_pool.len()];
    {
        let mut seen = vec![false; float_pool.len()];
        for (fi, fe) in function_emits.iter().enumerate() {
            for fx in &fe.fixups {
                if let FixupKind::FloatLoad { bits, width } = &fx.kind
                    && let Some(k) = float_pool.iter().position(|e| e == &(*bits, *width))
                    && !seen[k]
                {
                    seen[k] = true;
                    float_intro_fn[k] = fi;
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

    // LEDATA — CONST float-literal pool, the temps introduced by function 0
    // (the pre-_TEXT block). Each `$T` temp holds the IEEE bytes: a `float`
    // collapses the f64 value to f32 (4 bytes); a `double` keeps the full f64
    // (8 bytes). Little-endian. The pre-text float temps are emitted AFTER the
    // _DATA segment (MSC's segment order when both are present — fixture 2150),
    // below. Temps introduced by later functions are flushed mid-stream
    // (interleaved with the _TEXT runs).

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
                        GlobalInit::FloatBits(bits, width) => {
                            if *width == 4 {
                                data_bytes.extend_from_slice(
                                    &(f64::from_bits(*bits) as f32).to_bits().to_le_bytes());
                            } else {
                                data_bytes.extend_from_slice(&bits.to_le_bytes());
                            }
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
                        GlobalInit::Int(_) | GlobalInit::Byte(_) | GlobalInit::FloatBits(..) => {}
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

    // LEDATA — CONST float-literal pool (pre-text temps), emitted after _DATA.
    // MSC packs consecutive (adjacent-offset) float temps into a single LEDATA
    // (like strings), so emit them in contiguous-offset runs.
    {
        let mut pre: Vec<usize> = (0..float_pool.len()).filter(|&k| float_intro_fn[k] == 0).collect();
        pre.sort_by_key(|&k| float_offsets[k]);
        let mut i = 0;
        while i < pre.len() {
            let start_off = float_offsets[pre[i]];
            let mut buf: Vec<u8> = Vec::new();
            let mut expect = start_off;
            while i < pre.len() && float_offsets[pre[i]] == expect {
                let (bits, width) = float_pool[pre[i]];
                if width == 4 {
                    buf.extend_from_slice(&(f64::from_bits(bits) as f32).to_bits().to_le_bytes());
                } else {
                    buf.extend_from_slice(&bits.to_le_bytes());
                }
                expect += width;
                i += 1;
            }
            b.write_ledata16(3, u16::try_from(start_off).expect("CONST float offset fits"), &buf);
        }
    }

    // FIXUPP subrecord builder — shared across every _TEXT run. `off` is the
    // fixup's offset relative to its LEDATA record's start.
    //   ExtCall:     `84 off 56 <extdef_idx>` (self-rel to EXTDEF)
    //   StrLoad/FloatLoad: `c4 off 9c`        (seg-rel via DGROUP/CONST threads)
    //   FloatMarker: `c4 off 56 <extdef_idx>` (FxxRQQ emulator marker)
    //   GlobalAddr:  `c4 off 9d` (PUBDEF) / `c4 off 56 idx` (COMDEF)
    let build_fixup_subrecord = |off: u8, kind: &FixupKind, payload: &mut Vec<u8>| {
        match kind {
            FixupKind::ExtCall { target } => {
                let idx = *extdef_idx_of
                    .get(target)
                    .unwrap_or_else(|| panic!("EXTDEF index missing for `{target}`"));
                payload.extend_from_slice(&[0x84, off, 0x56, idx]);
            }
            FixupKind::StrLoad { .. } | FixupKind::FloatLoad { .. } => {
                payload.extend_from_slice(&[0xC4, off, 0x9C]);
            }
            FixupKind::FloatMarker { target } => {
                let idx = *extdef_idx_of
                    .get(*target)
                    .unwrap_or_else(|| panic!("EXTDEF index missing for FP marker `{target}`"));
                payload.extend_from_slice(&[0xC4, off, 0x56, idx]);
            }
            FixupKind::GlobalAddr { global_idx } => {
                if unit.globals[*global_idx].init.is_some() {
                    payload.extend_from_slice(&[0xC4, off, 0x9D]);
                } else {
                    let sym = symbol_name(&unit.globals[*global_idx].name);
                    let idx = *extdef_idx_of
                        .get(&sym)
                        .unwrap_or_else(|| panic!("EXTDEF index missing for COMDEF `{sym}`"));
                    payload.extend_from_slice(&[0xC4, off, 0x56, idx]);
                }
            }
            FixupKind::ExtData { target } => {
                let idx = *extdef_idx_of
                    .get(*target)
                    .unwrap_or_else(|| panic!("EXTDEF index missing for data extern `{target}`"));
                payload.extend_from_slice(&[0xC4, off, 0x56, idx]);
            }
            FixupKind::TuLocalCall { .. } => unreachable!(),
        }
    };

    // LEDATA — _TEXT segment. MSC emits one LEDATA per maximal contiguous run
    // of functions; a CONST float temp introduced by a later function splits
    // the run (its CONST LEDATA is flushed between the two _TEXT records,
    // mirroring the ASM's `_TEXT ENDS / CONST SEGMENT / _TEXT SEGMENT`
    // interleaving — fixture 1678). Each _TEXT LEDATA is immediately followed
    // by its own FIXUPP, with offsets relative to that record's start.
    let n_funcs = function_emits.len();
    let mut run_boundaries: Vec<usize> = vec![0];
    for i in 1..n_funcs {
        if float_intro_fn.iter().any(|&f| f == i) {
            run_boundaries.push(i);
        }
    }
    run_boundaries.push(n_funcs);
    for w in 0..run_boundaries.len().saturating_sub(1) {
        let start = run_boundaries[w];
        let end = run_boundaries[w + 1];
        let run_base = function_offsets[start];
        let mut run_bytes = Vec::new();
        for fi in start..end {
            run_bytes.extend_from_slice(&function_emits[fi].bytes);
        }
        let run_end_off = run_base + run_bytes.len();
        b.write_ledata16(1, u16::try_from(run_base).expect("text offset fits"), &run_bytes);
        // Fixups whose absolute (segment) offset lands inside this run,
        // re-based to the LEDATA start and sorted descending (MSC's order).
        let mut these: Vec<&ResolvedFixup> = ledata_fixups.iter()
            .filter(|rf| rf.ledata_offset >= run_base && rf.ledata_offset < run_end_off)
            .collect();
        these.sort_by(|a, b| b.ledata_offset.cmp(&a.ledata_offset));
        let mut payload = Vec::new();
        for rf in these {
            let off = u8::try_from(rf.ledata_offset - run_base).expect("fixup offset fits in u8");
            build_fixup_subrecord(off, &rf.kind, &mut payload);
        }
        if !payload.is_empty() {
            b.write_fixupp(&payload);
        }
        // Interleave the CONST float temps introduced by the function that
        // starts the next run.
        if end < n_funcs {
            for (k, &(bits, width)) in float_pool.iter().enumerate() {
                if float_intro_fn[k] == end {
                    let off = u16::try_from(float_offsets[k]).expect("CONST float offset fits");
                    if width == 4 {
                        b.write_ledata16(3, off, &(f64::from_bits(bits) as f32).to_bits().to_le_bytes());
                    } else {
                        b.write_ledata16(3, off, &bits.to_le_bytes());
                    }
                }
            }
        }
    }

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
    /// Global array element values keyed by (global_idx, byte_off).
    ga_known: std::collections::HashMap<(usize, u16), i32>,
    /// Globals that hold a 4-byte long. Skipped by the
    /// `Global(g) → IntLit(K)` substitution pass so compound updates
    /// like `g += K` and `g = g + K` keep `Global(g)` on the left for
    /// the long-specific assign codegen to recognize.
    long_globals: std::collections::HashSet<usize>,
    /// Locals that may have been mutated at runtime. The emit-time
    /// fold view drops these from `locals.inits` so post-mutation
    /// reads load from the slot rather than folding the declared init.
    mutated_locals: std::collections::HashSet<usize>,
    mutated_globals: std::collections::HashSet<usize>,
    /// Pointer-alias tracking: `int *p = &x` records `p -> x`, so a later
    /// `*p` (read or store) is rewritten to the aliased lvalue and folds /
    /// stores directly. Cleared at branch boundaries.
    ptr_alias: std::collections::HashMap<usize, AliasTarget>,
    /// Global-pointer alias tracking: `int *p; p = a;` (p a near GLOBAL pointer,
    /// a a global array) records `p -> Global(a)`, so `p[K]` reads/writes resolve
    /// to direct `a[K]` global addressing. Unlike the local single-use alias this
    /// persists until p is reassigned or a branch boundary. Keyed by global idx.
    ptr_alias_g: std::collections::HashMap<usize, AliasTarget>,
    /// Pointers whose alias was used in the current statement. MSC's alias is
    /// single-use: only the FIRST statement dereferencing `p` (after `p = &x`)
    /// is aliased; later `*p` derefs through the pointer. Drained after each
    /// top-level statement to drop those aliases.
    aliases_used: std::collections::HashSet<usize>,
    /// Pointer-value tracking: `p = &g[K]` records p holds the ADDRESS of
    /// (base, byte_offset). Lets `p - q` and `p == q` over same-base GLOBAL
    /// addresses fold to a compile-time constant. Cleared on reassignment and
    /// at branch boundaries.
    ptr_addr: std::collections::HashMap<usize, (AliasTarget, i32)>,
    /// Copy of local_specs for size checks during assignment propagation.
    local_specs: Vec<LocalSpec>,
    /// Parallel to globals: each global's element byte size (1 for char arrays
    /// /scalars, 2 for int, etc). Used to pick IndexByte vs Index and the byte
    /// offset when resolving a global-pointer subscript through `ptr_alias_g`.
    global_elem_sizes: Vec<usize>,
}

/// The lvalue a pointer local currently aliases (`&x`).
#[derive(Clone, Copy, Debug)]
enum AliasTarget {
    Local(usize),
    Global(usize),
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
